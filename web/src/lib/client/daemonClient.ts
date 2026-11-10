// Grant-authenticated HTTP client for the same-machine daemon (design §5.4).
// Holds the grant in memory only. Any 401 browser_grant_invalid/required
// clears the in-memory grant and fires `onRevoked` once.
import { DaemonError, GrantRevoked } from './errors';
import { clearGrant, isGrantLive, type Grant } from './grant';
import { HttpTransport, type RequestOptions } from './http';
import { completions, type Completion } from './catalogue';
import { Emitter } from './observable';
import { TICKET_RE, type TicketRoute } from './tickets';
import type {
  Annotations,
  BrowserTicketRequest,
  BrowserTicketResponse,
  Catalogue,
  CreateInstanceResponse,
  InstanceSummary,
  LaunchContextList,
  List,
  PromptKind,
  PromptView,
  SquadRun,
  SquadRunCanceled,
  SquadRunCreated,
  SquadTask,
  SquadTaskPatch,
  StatusResponse,
  TerminalRecord,
} from './types';

const REVOKED_CODES = new Set(['browser_grant_invalid', 'browser_grant_required']);

export class DaemonClient {
  private revoked = false;
  private readonly revokedEmitter = new Emitter<void>();
  private catalogueCache: Catalogue | null = null;

  constructor(
    readonly grant: Grant,
    readonly transport: HttpTransport = new HttpTransport(),
  ) {}

  get isRevoked(): boolean {
    return this.revoked || !isGrantLive(this.grant);
  }

  /** Fires once when the daemon rejects the grant (or a socket closes with 1008). */
  onRevoked(callback: () => void): () => void {
    if (this.revoked) {
      queueMicrotask(callback);
      return () => undefined;
    }
    return this.revokedEmitter.on(callback);
  }

  /** Called by sockets on close 1008; idempotent. */
  markRevoked(): void {
    if (this.revoked) return;
    this.revoked = true;
    this.catalogueCache = null;
    clearGrant();
    this.revokedEmitter.emit();
    this.revokedEmitter.clear();
  }

  private async request<T>(path: string, options: RequestOptions = {}): Promise<{ status: number; body: T; headers: Headers }> {
    if (this.isRevoked) throw new GrantRevoked();
    try {
      return await this.transport.json<T>(path, { ...options, bearer: this.grant.token });
    } catch (error) {
      if (error instanceof DaemonError && error.status === 401 && REVOKED_CODES.has(String(error.code))) {
        this.markRevoked();
        throw new GrantRevoked();
      }
      throw error;
    }
  }

  async status(): Promise<StatusResponse> {
    return (await this.request<StatusResponse>('/v1/status')).body;
  }

  async launchContexts(): Promise<LaunchContextList> {
    return (await this.request<LaunchContextList>('/v1/engine/launch_contexts')).body;
  }

  /** GET /v1/engine/catalogue with an in-memory ETag cache (If-None-Match → 304). */
  async catalogue(): Promise<Catalogue> {
    const cached = this.catalogueCache;
    const response = await this.request<Catalogue | null>('/v1/engine/catalogue', cached ? { headers: { 'If-None-Match': `"${cached.etag}"` } } : {});
    if (response.status === 304 && cached) return cached;
    if (!response.body) throw new DaemonError(response.status, 'invalid_response', 'empty catalogue');
    this.catalogueCache = response.body;
    return response.body;
  }

  /** TUI command-box completion over the cached catalogue (port of tui_completions). */
  async complete(partial: string): Promise<Completion[]> {
    return completions(await this.catalogue(), partial);
  }

  /** Layer 2 prompt wording (design §2.9, D8). */
  async prompt(kind: PromptKind, params: Record<string, string> = {}): Promise<PromptView> {
    const query = new URLSearchParams({ kind, ...params });
    return (await this.request<PromptView>(`/v1/engine/prompts?${query.toString()}`)).body;
  }

  async createInstance(annotations: Annotations = {}): Promise<CreateInstanceResponse> {
    return (await this.request<CreateInstanceResponse>('/v1/engine/instances', { method: 'POST', body: { annotations } })).body;
  }

  /** Instances this grant may see (web primaries only, design §2.3 D3). */
  async listInstances(): Promise<InstanceSummary[]> {
    const { body } = await this.request<List<InstanceSummary>>('/v1/engine/instances');
    return body.items.filter(item => item.primary_frontend_kind === 'web' || item.primary_frontend_kind === null);
  }

  /** One-use WebSocket upgrade ticket (design §2.7). */
  async ticket(route: TicketRoute, resourceId: string): Promise<string> {
    const body: BrowserTicketRequest = route === 'frontend' ? { route, instance_id: resourceId } : { route, terminal_id: resourceId };
    const response = await this.request<BrowserTicketResponse>('/v1/browser-tickets', { method: 'POST', body });
    if (!TICKET_RE.test(response.body.ticket)) throw new DaemonError(response.status, 'invalid_response', 'malformed ticket');
    return response.body.ticket;
  }

  async terminals(ownerId: string): Promise<TerminalRecord[]> {
    const query = new URLSearchParams({ owner_id: ownerId });
    return (await this.request<List<TerminalRecord>>(`/v1/terminals?${query.toString()}`)).body.items;
  }

  async terminal(id: string): Promise<TerminalRecord> {
    return (await this.request<TerminalRecord>(`/v1/terminals/${encodeURIComponent(id)}`)).body;
  }

  /** `/v1/squad/*` on the same browser listener (design D11). */
  readonly squad = {
    tasks: async (): Promise<SquadTask[]> => (await this.request<List<SquadTask>>('/v1/squad/tasks')).body.items,
    task: async (id: string): Promise<SquadTask> => (await this.request<SquadTask>(`/v1/squad/tasks/${encodeURIComponent(id)}`)).body,
    runs: async (id: string): Promise<SquadRun[]> => (await this.request<List<SquadRun>>(`/v1/squad/tasks/${encodeURIComponent(id)}/runs`)).body.items,
    patch: async (id: string, patch: SquadTaskPatch): Promise<SquadTask> =>
      (await this.request<SquadTask>(`/v1/squad/tasks/${encodeURIComponent(id)}`, { method: 'PATCH', body: patch })).body,
    trigger: async (id: string): Promise<SquadRunCreated> =>
      (await this.request<SquadRunCreated>(`/v1/squad/tasks/${encodeURIComponent(id)}/runs`, { method: 'POST', body: {} })).body,
    cancelRun: async (runId: string): Promise<SquadRunCanceled> =>
      (await this.request<SquadRunCanceled>(`/v1/squad/runs/${encodeURIComponent(runId)}/cancel`, { method: 'POST', body: {} })).body,
    delete: async (id: string): Promise<void> => {
      await this.request<null>(`/v1/squad/tasks/${encodeURIComponent(id)}`, { method: 'DELETE' });
    },
  };
}
