function ipv4(value: string): string | null {
  const parts = value.split('.');
  if (parts.length !== 4 || parts.some(p => !/^(0|[1-9][0-9]{0,2})$/.test(p) || Number(p) > 255)) return null;
  return parts.map(Number).join('.');
}

// No permissive URL parser: reject ports, brackets, scopes, lists, shorthand
// IPv4, octal and whitespace. Full IPv6 is represented by eight padded groups.
export function canonicalIp(value: string | null): string | null {
  if (!value || value.length > 45) return null;
  if (!value.includes(':')) return ipv4(value);
  let address = value;
  if (address.includes('.')) {
    const split = address.lastIndexOf(':');
    const dotted = ipv4(address.slice(split + 1));
    if (!dotted) return null;
    const bytes = dotted.split('.').map(Number);
    address = address.slice(0, split + 1) + ((bytes[0]! << 8) | bytes[1]!).toString(16)
      + ':' + ((bytes[2]! << 8) | bytes[3]!).toString(16);
  }
  if (!/^[0-9a-fA-F:]+$/.test(address)) return null;
  const halves = address.split('::');
  if (halves.length > 2) return null;
  const left = halves[0] ? halves[0].split(':') : [];
  const right = halves[1] ? halves[1].split(':') : [];
  if ([...left, ...right].some(p => !/^[0-9a-fA-F]{1,4}$/.test(p))) return null;
  const count = left.length + right.length;
  if (halves.length === 1 ? count !== 8 : count >= 8) return null;
  const groups = halves.length === 1 ? left : [...left, ...Array<string>(8 - count).fill('0'), ...right];
  return groups.map(p => p.toLowerCase().padStart(4, '0')).join(':');
}
