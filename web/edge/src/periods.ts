export interface Periods { day: string; month: string }
export function periodsAt(now: number): Periods {
  const day = new Date(now).toISOString().slice(0, 10);
  return { day, month: day.slice(0, 7) };
}

// Calendar arithmetic clamps end-of-month dates (e.g. Jan 31 + 13 months).
// Rows expire 13 calendar months after their UTC period end.
export function expiresAt(kind: 'day' | 'month', period: string): number {
  const end = new Date(kind === 'day' ? `${period}T00:00:00Z` : `${period}-01T00:00:00Z`);
  if (kind === 'day') end.setUTCDate(end.getUTCDate() + 1);
  else end.setUTCMonth(end.getUTCMonth() + 1);
  const day = end.getUTCDate();
  end.setUTCDate(1);
  end.setUTCMonth(end.getUTCMonth() + 13);
  const lastDay = new Date(Date.UTC(end.getUTCFullYear(), end.getUTCMonth() + 1, 0)).getUTCDate();
  end.setUTCDate(Math.min(day, lastDay));
  return end.getTime();
}

export function nextMidnight(now: number): number {
  const date = new Date(now);
  return Date.UTC(date.getUTCFullYear(), date.getUTCMonth(), date.getUTCDate() + 1);
}
