**Title:** `network: should DomainSuffix allows require a DNS binding for the SNI name itself?`

**Labels:** 💗 enhancement (or a discussion, if you'd rather)

---

## Summary

For a `DomainSuffix` allow rule with an SNI hostname source, the policy currently
admits the connection if both of these hold:

- the SNI matches the suffix;
- the dialed IP was resolved for **any** name under the suffix.

This is in `lib/model/policy/types.rs`, `matches_egress_destination_with_source`:

```rust
let cache_matches = || {
    shared.any_resolved_hostname(addr, |hostname| {
        matches_suffix(hostname, suffix.as_str())
    })
};
```

By contrast, an exact `Domain` rule requires the cache to bind that exact name. We'd
like to ask whether suffix rules should also require a binding for the SNI name
itself:

```rust
let cache_matches = || shared.any_resolved_hostname(addr, |hostname| hostname == name);
```

## Why we're asking

We know the current behaviour is deliberate. The test
`hostname_source_sni_domain_suffix_allows_sibling_cache_binding` describes it as a
"Real shared-CDN case": an IP resolved as `other.pythonhosted.org` serving
`files.pythonhosted.org`.

The cost is that inside an allowed suffix, the guest picks both the SNI and the IP.
It can resolve one name and then claim another name under the suffix against that
IP, even though the claimed name was never resolved there.

This matters more when the SNI is the authority being trusted, as with the opt-in
proposed in #<issue-03-number>, than with interception. We carry the exact-binding
version as a patch on `microsandbox-network 0.7.8` and apply it in every mode. In
practice the guest resolves the name it is about to connect to, so a legitimate
client almost always has an exact binding. We haven't seen it break CDN-hosted
registries.

## Options

1. **Require the exact binding for suffix allows in every mode.** This is what we
   carry. The sibling test changes to assert refusal. Deny rules are unaffected,
   because they still match on the SNI alone.
2. **Require it only when `strict_sni` is on**, if #<issue-03-number> lands. Current
   behaviour stays for everyone else, and the tighter rule applies only where SNI is
   the trust anchor.
3. **Keep the current behaviour.** We'd then carry the tightening ourselves, or scope
   it to our own option.

We're happy to send a PR for 1 or 2, with a test for both the refused sibling case
and the allowed exact-binding case. Option 2 is the least disruptive if you want to
keep the shared-CDN behaviour by default.
