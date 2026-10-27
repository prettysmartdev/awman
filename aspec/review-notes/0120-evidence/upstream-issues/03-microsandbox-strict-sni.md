**Title:** `feat(network): opt-in strict_sni — accept visible SNI with an exact DNS binding under strict mode`

**Labels:** 💗 enhancement

---

## Summary

Under `strict` (the default since 0.7.8), TCP egress that is allowed only by a
hostname rule must carry inspectable application authority. Opaque TLS to a
hostname-allowed destination is refused unless the gateway intercepts it
(`strict_hostname_allow_is_opaque` in `lib/engine/tcp/proxy.rs`). So an embedder that
wants hostname allow-lists and strict mode has to install an interception CA in the
guest.

We'd like a default-off option, `strict_sni`, as a middle ground. When enabled, a TLS
connection is admitted end to end, with no interception, if both of these hold:
- its visible ClientHello SNI matches an allow rule;
- the destination IP is bound to that exact name in the DNS cache.

Every case strict mode can't verify is still refused.

## Motivation

We embed Microsandbox in [awman](https://github.com/prettysmartdev/awman), which runs code
agents in microVMs with a hostname allow-list (for example the model provider's API
host and a package registry). As a matter of policy, awman never installs an
interception CA of its own. The guest trusts only its image's CA bundle, plus the
host's roots if the user opts in. That leaves two choices today:

- **`strict(false)`:** hostname rules can then also be satisfied by the DNS cache
  alone, with no SNI. That is weaker than we want.
- **`strict(true)`:** this needs TLS interception, which we deliberately don't do.

With `strict(true).strict_sni(true)`:

| Connection | Result |
|---|---|
| Allowed visible SNI plus an exact DNS binding for that name to the dialed IP | Allowed, end to end, no interception |
| No SNI, including ECH-only ClientHellos | Refused |
| SNI not in the allow-list | Refused |
| SNI allowed, but no DNS binding for that name to the IP | Refused |

The boundary is only what is visible. Encrypted HTTP `Host`/`:authority`, domain
fronting behind an allowed server, and ECH's inner name are all outside it. An
allowed ECH outer name only shows that the outer name was used. QUIC isn't inspected,
so name rules should stay TCP-only. We'd document all of this on the option.

## Proposed change

The network and types crates both need changes. They should be one PR so the option
can't be dropped in between.

- **`microsandbox-network`**
  - Add `strict_sni: bool` to `NetworkConfig`, with `#[serde(default)]` so it is
    `false`. Missing in old configs means off.
  - Add `NetworkBuilder::strict_sni(bool)`.
  - Thread it through the netstack poll loop to `TcpProxy`.
  - In `TcpProxy`, skip the opaque-TLS refusal only when `strict_sni` is on and an SNI
    is present. Policy evaluation has already matched that SNI against the DNS binding.
- **`microsandbox-types`**
  - Add `strict_sni` (default `false`, omitted when false) to `NetworkSpec`.
  - Without this, `SandboxBuilder` sends the network config through `NetworkSpec` and
    silently drops the option.
- **Runtime and SDK compatibility**, following the `tcp_accept_queue_size`
  precedent in `COMPATIBILITY.md`:
  - `msb __launch-protocol` advertises a `strict_sni` capability.
  - The SDK probes for it only when the option is set.
  - v0.6.x contracts and managed cloud refuse it before launch.

  That way an older peer never silently drops it.

Nothing changes for anyone who doesn't set the option.

## Tests

We carry this on `microsandbox-network 0.7.8` and `microsandbox-types 0.7.8`. The
tests are:

- **End-to-end handshake:** a real Rustls handshake and application-data exchange
  through `TcpProxy` to an allowed server, with no interception.
- **Refusals that never dial upstream:** missing SNI, an unlisted SNI on a shared IP,
  an allowed SNI with no DNS binding, and an SNI that only a sibling name's binding
  covers. Each must be refused before any upstream dial.
- **Config compatibility:**
  - an old config without the field deserializes with it off;
  - builder → JSON → `NetworkConfig` keeps the field;
  - the `SandboxBuilder` network config → `NetworkSpec` → runtime config round trip
    keeps it.
- **Existing behaviour:** the strict-mode deny tests still pass.

It also runs in real guests on Apple Silicon. Those tests cover allowed HTTPS through
an allow-list, denied egress (DNS, direct IP and proxy bypass), and isolation of
network policy between concurrent VMs.

## Related

The fourth refusal case above, a sibling binding, depends on how suffix rules use the
DNS cache. That changes existing behaviour, so it's raised separately in
#<issue-04-number>. This option can land with either answer there.

## Questions

- Is this a direction you'd accept, or do you see strict mode as interception-only by
  design?
- Should it be a separate flag, as proposed, or a mode, for example
  `strict = "inspect" | "sni"`?
