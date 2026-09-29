use strict; use warnings;
for my $p (glob('aspec/work-items/0119-*.md'),glob('aspec/work-items/0121-*.md'),glob('aspec/work-items/0122-*.md'),glob('aspec/work-items/0123-*.md')) {
 open my $f,'<',$p or die $!; local $/; my $s=<$f>; my $yes=()=$s =~ /^- \[x\]/gmi; my $no=()=$s =~ /^- \[ \]/gm;
 print "$p: checked=$yes open=$no\n";
 die 'Mandatory acceptance falsely closed' if $yes;
}
