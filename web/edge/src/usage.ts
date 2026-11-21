import { DurableObject } from 'cloudflare:workers';
import { addDigest, estimate, newSketch } from './hll';
import { expiresAt, nextMidnight, periodsAt, type Periods } from './periods';
import type { Digests } from './privacy';

export interface UsageRow {
  kind: 'day' | 'month';
  period: string;
  pageLoads: number;
  missingIpCount: number;
  distinctIpEstimate: number;
  // Present only for an open period; never an individual HMAC or address.
  sketch?: Uint8Array;
}
export interface UsageExport {
  timezone: 'UTC';
  distinctIpLabel: 'approximate distinct IPs';
  rows: Omit<UsageRow, 'sketch'>[];
}
export interface CountInput { periods: Periods; digests: Digests }
export type CountResult = 'counted' | 'period_changed' | 'unavailable';

export class UsageCounter extends DurableObject {
  // Public RPC surface is binding-only; no HTTP endpoint accepts visitor data.
  async record(input: CountInput): Promise<CountResult> {
    try {
      const validDigest = (v: Uint8Array | null) => v === null || (v instanceof Uint8Array && v.length === 32);
      if (!validDigest(input.digests.day) || !validDigest(input.digests.month)
        || (input.digests.day === null) !== (input.digests.month === null)) return 'unavailable';
      return await this.ctx.storage.transaction(async txn => {
        const now = Date.now();
        const current = periodsAt(now);
        // Do not reopen a finalized sketch if edge hashing crossed midnight.
        // No write occurred; the edge may resample time and hash once more.
        if (input.periods.day !== current.day || input.periods.month !== current.month) return 'period_changed';
        await this.maintain(txn, now);
        for (const kind of ['day', 'month'] as const) {
          const period = current[kind];
          const key = `${kind}:${period}`;
          const row = await txn.get<UsageRow>(key) ?? {
            kind, period, pageLoads: 0, missingIpCount: 0, distinctIpEstimate: 0, sketch: newSketch(),
          };
          row.pageLoads++;
          const digest = input.digests[kind];
          if (digest === null) row.missingIpCount++;
          else addDigest(row.sketch!, digest);
          row.distinctIpEstimate = estimate(row.sketch!);
          await txn.put(key, row);
        }
        await txn.setAlarm(nextMidnight(now));
        return 'counted';
      });
    } catch {
      // Never serialize runtime exceptions, RPC arguments or digests.
      return 'unavailable';
    }
  }

  private async maintain(txn: DurableObjectTransaction, now: number): Promise<void> {
    const current = periodsAt(now);
    const rows = await txn.list<UsageRow>();
    for (const [key, row] of rows) {
      if (expiresAt(row.kind, row.period) <= now) {
        await txn.delete(key);
      } else if (row.period < current[row.kind] && row.sketch) {
        row.distinctIpEstimate = estimate(row.sketch);
        delete row.sketch;
        await txn.put(key, row);
      }
    }
  }

  async exportAggregates(): Promise<UsageExport | null> {
    try {
      return await this.ctx.storage.transaction(async txn => {
        const now = Date.now();
        await this.maintain(txn, now);
        const rows = await txn.list<UsageRow>();
        if (rows.size) await txn.setAlarm(nextMidnight(now));
        return {
          timezone: 'UTC', distinctIpLabel: 'approximate distinct IPs',
          rows: [...rows.values()].map(row => ({
            kind: row.kind, period: row.period, pageLoads: row.pageLoads,
            missingIpCount: row.missingIpCount, distinctIpEstimate: row.distinctIpEstimate,
          })),
        };
      });
    } catch { return null; }
  }

  async alarm(): Promise<void> {
    // Alarm retry is safe: it never increments counters. A failed maintenance
    // transaction is retried by the runtime, with a fixed sanitized exception.
    try {
      await this.ctx.storage.transaction(async txn => {
        const now = Date.now();
        await this.maintain(txn, now);
        if ((await txn.list()).size) await txn.setAlarm(nextMidnight(now));
      });
    } catch { throw new Error('Usage maintenance unavailable'); }
  }
}
