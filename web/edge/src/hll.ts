// p=12: 4,096 one-byte registers, asymptotic standard error 1.04/sqrt(m)
// = 1.625%. HMAC-SHA-256 provides uniform independent register/rank bits.
export const HLL_PRECISION = 12;
export const HLL_REGISTERS = 1 << HLL_PRECISION;
export function newSketch(): Uint8Array { return new Uint8Array(HLL_REGISTERS); }

export function addDigest(registers: Uint8Array, digest: Uint8Array): void {
  if (registers.length !== HLL_REGISTERS || digest.length !== 32) throw new Error('Invalid sketch input');
  const index = (digest[0]! << 4) | (digest[1]! >>> 4);
  let rank = 1;
  for (let bit = HLL_PRECISION; bit < 256; bit++) {
    if ((digest[bit >>> 3]! & (1 << (7 - (bit & 7)))) !== 0) break;
    rank++;
  }
  registers[index] = Math.max(registers[index]!, rank);
}

export function estimate(registers: Uint8Array): number {
  let zeros = 0;
  let inverse = 0;
  for (const register of registers) {
    inverse += 2 ** -register;
    if (register === 0) zeros++;
  }
  const m = HLL_REGISTERS;
  const raw = (0.7213 / (1 + 1.079 / m)) * m * m / inverse;
  return raw <= 2.5 * m && zeros > 0 ? m * Math.log(m / zeros) : raw;
}
