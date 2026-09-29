use strict; use warnings; use File::Basename qw(dirname);
my @files=(glob('aspec/work-items/0119-*.md'),glob('aspec/work-items/0120-*.md'),glob('aspec/work-items/0121-*.md'),glob('aspec/work-items/0122-*.md'),glob('aspec/work-items/0123-*.md'),'docs/11-runtimes.md','aspec/architecture/design.md','aspec/architecture/security.md','third_party/README.md','NOTICE.third-party.md','tools/oci-runtime-spike/sqlite-resolution/README.md');
my $missing=0;
for my $p(@files){open my $f,'<',$p or die $!;local $/;my $s=<$f>;while($s =~ /\]\(([^)]+)\)/g){my $link=$1;next if $link =~ /^(?:https?:|#)/;$link =~ s/#.*$//;my $path=dirname($p).'/'.$link;if(-e $path){print "EXISTS $path\n"}else{print "MISSING $path\n";$missing++}}}
exit($missing?1:0);
