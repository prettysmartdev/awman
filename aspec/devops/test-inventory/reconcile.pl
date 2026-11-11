#!/usr/bin/env perl
# Static phase-0 reconciliation. Run from repository root; preserves existing IDs.
# Does not claim to expand Rust macros or evaluate cfg. Cargo lists are separate.
use strict;
use warnings;
use File::Find;
use JSON::PP;
use Digest::SHA qw(sha256_hex);
my $dir='aspec/devops/test-inventory';
sub csv { join(',', map {my $s=defined($_)?"$_":''; $s =~ s/"/""/g; '"'.$s.'"'} @_)."\n" }
sub parse {
 my ($s)=@_; chomp $s; my @r;
 while ($s =~ /\G(?:"((?:[^"]|"")*)"|([^,]*))(?:,|$)/gc) {
  my $v=defined($1)?$1:$2; $v =~ s/""/"/g; push @r,$v;
  last if pos($s)>=length($s);
 } return @r;
}
sub readcsv {my($p)=@_;open my $f,'<',$p or die $!;my $header=<$f>;die "empty CSV $p" unless defined $header;my @h=parse($header);my @rows;while(<$f>){my @v=parse($_);die "Bad CSV $p $." unless @v==@h;my %r;@r{@h}=@v;push @rows,\%r;}return (\@h,\@rows);}
sub writecsv {my($p,$h,$r)=@_;open my $f,'>',$p or die $!;print $f csv(@$h);for(@$r){print $f csv(@{$_}{@$h})}}
my @files;find({no_chdir=>1,wanted=>sub {push @files,$File::Find::name if -f && /\.rs$/}},qw(src crates tests));@files=sort @files;
my ($h,$rows)=readcsv("$dir/rust-tests.csv");my %known=map {("$_->{file}:$_->{line}"=>1)} @$rows;
my %sample;for(@$rows){$sample{$_->{file}} //= $_;}
my($plain,$args,$added)=(0,0,0);
for my $file(@files){open my $f,'<',$file or die $!;my @l=<$f>;for my $i(0..$#l){
 next unless $l[$i] =~ /^\s*#\[(?:tokio::)?test(?:\]|\()/;
 $l[$i] =~ /test\(/ ? $args++ : $plain++;
 next if $known{"$file:".($i+1)};
 my $name='unknown';for my $j($i+1..($i+20<$#l?$i+20:$#l)){if($l[$j]=~/\bfn\s+(\w+)/){$name=$1;last}}
 my %r=map {($_=>'unknown; static annotation')} @$h;
 if($sample{$file}){@r{qw(target layer current_caller suggested_target_tier)}=@{$sample{$file}}{qw(target layer current_caller suggested_target_tier)}}
 @r{qw(id test_name file line disposition)}=("$file:".($i+1)."::$name",$name,$file,$i+1,'pending');
 $r{duration}='unknown';$r{regression_commit}='unknown beyond source';
 push @$rows,\%r;$added++;
}}
# Recompute descriptive hints uniformly, including parameterized test attributes.
# Hints are explicitly static, not semantic claims about execution.
for my $r(@$rows){
 my $p=$r->{file}; my $layer=$p=~m{(?:daemon|squad|worker|awman-wsp)}?'daemon-squad-worker':
 $p=~m{(?:builtin_runtime|awman-host-shims)}?'guest':$p=~m{(?:artifact|release)}?'artifact':
 $p=~m{(?:/data/|/data_layer/)}?'0 data':$p=~m{(?:/engine/|/engine_parity/|/oci_import/)}?'1 engine':
 $p=~m{(?:/command/|/commands/|workflow_prerequisites)}?'2 command':
 $p=~m{(?:/frontend/|awman-cli|parity|binary_smoke)}?'3 frontend':'unclassified';
 $r->{layer}=$layer;$r->{suggested_target_tier}=$layer;
 $r->{target}=$p=~m{^src/}?'awman unit; cfg/feature list pending':$p=~m{^crates/([^/]+)/src/}?"$1 unit; cfg/feature list pending":'integration; see cargo-targets.csv; exact module reachability pending';
 $r->{current_caller}='Make/Actions selection inferred only; see make-targets.csv and workflows.csv; compiled membership pending';
 open my $f,'<',$p or die $!;my @l=<$f>;my $end=$r->{line}+35<$#l?$r->{line}+35:$#l;my $window=join('',@l[$r->{line}-1..$end]);
 my %env=map {($_=>1)} ($window=~/\b(AWMAN_TEST_[A-Z0-9_]+)\b/g);
 $r->{required_environment}=keys(%env)?join(';',sort keys %env):'unknown; no local-window hint';
 my @effects;push @effects,'process' if $window=~/Command|spawn/;push @effects,'network' if $window=~/Tcp|Socket|Listener/;push @effects,'filesystem' if $window=~/tempdir|fs::/;
 $r->{side_effects}=(@effects?join(';',@effects):'unknown').'; static 36-line window';
 $r->{early_return_skip}=$window=~/skip|SKIP/?'skip text in static window; review required':'unknown; no local-window skip text';
 my @sha=$window=~/(?:Guards:|regression|commit)\s+([0-9a-f]{8})\b/ig;$r->{regression_commit}=@sha?join(';',@sha).'; source citation, not independently verified':'unknown beyond local window';
}
@$rows=sort {$a->{file} cmp $b->{file} || $a->{line}<=>$b->{line}} @$rows;
writecsv("$dir/rust-tests.csv",$h,$rows);
my @ff;find({no_chdir=>1,wanted=>sub {push @ff,$File::Find::name if -f}},qw(tests crates/awman-cli/tests crates/awman-wsp/tests));
open my $ff,'>',"$dir/test-files.csv" or die $!;print $ff csv(qw(id path sha256 role disposition));
for my $p(sort @ff){open my $f,'<',$p or die $!;binmode $f;local $/;my $b=<$f>;print $ff csv($p,$p,sha256_hex($b),$p=~/\.rs$/?'test/helper/driver source':'fixture/data','pending')}
close $ff;
my @docs;for my $p(@files){open my $f,'<',$p or die $!;my $inside=0;my $line=0;while(<$f>){$line++;next unless /^\s*\/\/[!\/]\s*```(\S*)/;my $lang=$1;if($inside){$inside=0;next}$inside=1;push @docs,{id=>"$p:$line\::doc-fence",file=>$p,line=>$line,language=>$lang||'rust',discovery_status=>$lang=~/^(json|text|sh|bash|toml|yaml|console)$/?'non-Rust documentation fence; not executable':'Rust fence; native rustdoc list pending',disposition=>'pending',suggested_target_tier=>'unit'};}}
writecsv("$dir/doctests.csv",[qw(id file line language discovery_status disposition suggested_target_tier)],\@docs);
open my $mf,'<',"$dir/cargo-metadata-resolved.json" or die $!;my $meta=decode_json(do{local $/;<$mf>});
my ($rh,$regs)=readcsv("$dir/cargo-registrations.csv");my %reg=map {("$_->{package}:$_->{kind}:$_->{name}"=>$_)} @$regs;
open my $tf,'>',"$dir/cargo-targets.csv" or die $!;print $tf csv(qw(package kind name path required_features test doctest discovery));
my $targets=0;for my $p(@{$meta->{packages}}){for my $t(@{$p->{targets}}){my $path=$t->{src_path};$path=~s{^/workspace/}{};my $kind=join('+',@{$t->{kind}});print $tf csv($p->{name},$kind,$t->{name},$path,join('+',@{$t->{'required-features'}||[]}),$t->{test}?1:0,$t->{doctest}?1:0,'cargo metadata --no-deps --locked');$targets++;
 next unless $kind eq 'test' || $kind eq 'example';my $key="$p->{name}:$kind:$t->{name}";
 if($reg{$key}){my $r=$reg{$key};$r->{path}=$path;my $manifest=$p->{manifest_path};$manifest=~s{^/workspace/}{};open my $f,'<',$manifest or die $!;my $toml=do{local $/;<$f>};my $explicit=grep {/\bname\s*=\s*"\Q$t->{name}\E"/} ($toml=~/\[\[\Q$kind\E\]\]([^\[]*)/g);$r->{id}=$explicit?"$manifest\::$kind\::$t->{name}":"cargo-discovery::$key";$r->{current_caller}=$explicit?'Explicit Cargo target; Make/Actions filter membership pending':'Cargo automatic target discovery; exact CI selection pending';next}
 my %r=map {($_=>'unknown')} @$rh;@r{qw(id kind package name path current_caller disposition suggested_target_tier)}=("cargo-discovery::$key",$kind,$p->{name},$t->{name},$path,'Cargo automatic target discovery; exact CI selection pending','pending','component');push @$regs,\%r;
}}
writecsv("$dir/cargo-registrations.csv",$rh,$regs);
close $tf;
my ($lh,$ledger)=readcsv("$dir/ledger-crosswalk.csv");for my $r(@$ledger){$r->{mapped_inventory_entries}=~s/tests file inventory required/test-files.csv/;$r->{mapped_inventory_entries}=~s{aspec/devops/documentation.csv}{documentation.csv};}writecsv("$dir/ledger-crosswalk.csv",$lh,$ledger);
# Read and validate every CSV, all referenced paths/IDs, not only a sample.
for my $p(sort glob("$dir/*.csv")){my($ch,$cr)=readcsv($p);my %ids;for my $r(@$cr){die "duplicate id $p $r->{id}" if defined($r->{id}) && $ids{$r->{id}}++;if($p=~m{/(rust-tests|inline-test-modules|doctests)\.csv$}){die "missing file $r->{file}" unless -f $r->{file};}}print "$p: ".scalar(@$cr)." rows\n";}
for my $r(@$ledger){for my $p(split /;\s*/,$r->{mapped_inventory_entries}){die "unmapped ledger dataset $p" unless -f "$dir/$p"}}
die 'static annotation count mismatch' unless @$rows==$plain+$args;
die 'WI section 9 crosswalk must contain 15 rows' unless @$ledger==15;
print "plain_attributes=$plain parameterized_attributes=$args total=".($plain+$args)." added=$added targets=$targets files=".scalar(@ff)."\n";
