use strict; use warnings;
sub edit {my($p,$fn)=@_;open my $f,'<',$p or die $!;local $/;my $s=<$f>;close $f;$fn->(\$s);open $f,'>',$p or die $!;print $f $s;close $f;}
edit('.github/workflows/release.yml',sub{my $s=shift;
$$s =~ s/release-macos-builtin-signed/release-macos-builtin/g;
$$s =~ s/Apple Silicon signed\/notarized builtin artifact and boot gate/Apple Silicon builtin artifact and boot gate/;
$$s =~ s/hypervisor, signing/hypervisor/;
$$s =~ s/      - name: Require signing and notarization credentials\n.*?(?=      - name: Prepare native verified payload)//s;
$$s =~ s/      - name: Sign, notarize and staple the actual awman executable\n.*?(?=      - name: Trace and boot the exact distributed artifact)/      - name: Prepare the distribution binary (no signing or notarization)\n        run: |\n          artifact="\$CARGO_TARGET_DIR\/aarch64-apple-darwin\/release\/awman"\n          cp "\$artifact" awman-macos-arm64\n          shasum -a 256 awman-macos-arm64 > release-macos-awman.sha256\n/s;
$$s =~ s/awman-macos-arm64-builtin-signed/awman-macos-arm64-builtin/g;
$$s =~ s/^            awman-macos-arm64\.pkg\n//m;
$$s =~ s/ -o -name 'awman-macos-arm64\.pkg'//;
# Run the boot/trace on the actual copied distribution file.
$$s =~ s{bash tools/(release-artifact-check|measure-release-artifact)\.sh "\$CARGO_TARGET_DIR/aarch64-apple-darwin/release/awman"}{bash tools/$1.sh "\$PWD/awman-macos-arm64"}g;
});
edit('.github/workflows/test.yml',sub{my $s=shift;
$$s =~ s/^.*command -v codesign.*\n//m;
$$s =~ s/      - name: Grant the hypervisor entitlement to the guest driver \(macOS\)\n.*?(?=      - name: Builtin tests including real guests)//s;
});
edit('tools/native-builtin-ci.sh',sub{my $s=shift;
$$s =~ s/^.*command -v codesign.*\n//m;
$$s =~ s/if \[ "\$\(uname -s\)" = Darwin \]; then\n  entitlement=.*?\nfi\n//s;
});
edit('tests/builtin_runtime/gate.rs',sub{my $s=shift;
$$s =~ s{/// Hypervisor.framework support and the hypervisor entitlement on the binary.}{/// Hypervisor.framework support. Actual boot verifies OS permission to run a VM.};
$$s =~ s/        let signed = std::process::Command::new\("codesign"\).*?\n    }\n    #\[cfg\(not/        let _ = binary;\n        Ok(())\n    }\n    #[cfg(not/s;
$$s =~ s{    // The driver \(not the awman test copy\) is what boots guests, so it is what\n    // must carry the macOS hypervisor entitlement.}{    // Check the host for the driver that will boot the guest. Actual boot\n    // remains mandatory; these preflights do not prove OS access.};
});
for my $p('tools/oci-runtime-spike/strict-embed/mac-checks.sh','tools/oci-runtime-spike/sqlite-resolution/checks.sh','tools/oci-runtime-spike/mac-checks.sh') {
edit($p,sub{my $s=shift;$$s =~ s/^.*(?:^codesign |    codesign |run_case \S+ 0 codesign ).*\n//mg;$$s =~ s/ patch codesign otool/ patch otool/;$$s =~ s/ shasum codesign perl/ shasum perl/;});
}
