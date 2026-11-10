export function auditHtml(html: string): { problems: string[]; localReferences: string[] };
export function wasmPolicyProblems(assetPaths: string[], csp: string): string[];
export const ALLOWED_CONFIG_KEYS: string[];
export function configKeyProblems(config: Record<string, unknown>): string[];
export function auditSvg(text: string): string[];
