use strict; use warnings;
sub edit { my($file,$from,$to)=@_; local $/; open my $in,'<',$file or die $!; my $s=<$in>; close$in; index($s,$from)>=0 or die "not found: $file $from"; $s =~ s/\Q$from\E/$to/g; open my $out,'>',$file or die$!; print $out $s; }
my $p='third_party/microsandbox-network-0.7.2/lib';
edit("$p/model/config/types.rs", '    pub strict: bool,', '    pub strict: bool,

    /// Permit visible TLS SNI plus an exact DNS binding as strict authority.
    /// Opt-in for end-to-end TLS without interception. Encrypted HTTP authority
    /// is outside this boundary; absent SNI (including ECH-only) is refused.
    #[serde(default)]
    pub strict_sni: bool,');
edit("$p/model/config/types.rs", '            strict: false,', '            strict: false,
            strict_sni: false,');
edit("$p/model/config/builder.rs", '    /// Add a secret via a closure builder.', '    /// Accept visible SNI and its DNS binding when strict mode is enabled.
    /// This does not inspect encrypted HTTP authorities or disable strict mode.
    pub fn strict_sni(mut self, enabled: bool) -> Self {
        self.config.strict_sni = enabled;
        self
    }

    /// Add a secret via a closure builder.');
edit("$p/engine/network.rs", '        let strict = config.strict;', '        let strict = config.strict;
        let strict_sni = config.strict_sni;');
edit("$p/engine/network.rs", '                        strict,', '                        strict,
                        strict_sni,');
edit("$p/engine/netstack/poll.rs", '    strict: bool,', '    strict: bool,
    strict_sni: bool,');
edit("$p/engine/netstack/poll.rs", '                connection_outbound_proxy,
            );
            tokio_handle.spawn(proxy.run());', '                connection_outbound_proxy,
            ).with_strict_sni(strict_sni);
            tokio_handle.spawn(proxy.run());');
edit("$p/engine/tcp/proxy.rs", '    strict: bool,
    proxy_connect:', '    strict: bool,
    strict_sni: bool,
    proxy_connect:');
edit("$p/engine/tcp/proxy.rs", '            strict,
            proxy_connect,
            outbound_proxy,
        }
    }', '            strict,
            strict_sni: false,
            proxy_connect,
            outbound_proxy,
        }
    }

    pub(crate) fn with_strict_sni(mut self, enabled: bool) -> Self {
        self.strict_sni = enabled;
        self
    }');
edit("$p/engine/tcp/proxy.rs", '            strict,
            proxy_connect,
            outbound_proxy,
        } = self;', '            strict,
            strict_sni,
            proxy_connect,
            outbound_proxy,
        } = self;');
edit("$p/engine/tcp/proxy.rs", '                    if strict_hostname_allow_is_opaque(', '                    // Policy has already checked SNI against the exact DNS
                    // binding. The explicit opt-in admits end-to-end TLS; the
                    // ordinary strict/interception behavior remains unchanged.
                    if !(strict_sni && sni.is_some()) && strict_hostname_allow_is_opaque(');
# Suffix allows must bind the actual SNI, not a sibling in the same suffix.
edit("$p/model/policy/types.rs", '                        matches_suffix(hostname, suffix.as_str())', '                        hostname == name');
edit('src/engine/container/builtin/msb_driver.rs', '.strict(plan.strict)', '.strict(plan.strict)
                        .strict_sni(plan.strict)');
