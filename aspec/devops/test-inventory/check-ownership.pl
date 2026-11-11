#!/usr/bin/env perl
# Validate the reviewed ordered resolver against repository files and rule roots.
use strict; use warnings;
my $map=shift||'/awman/context/workflow/ownership.md';
open my $f,'<',$map or die $!;my(@rules,%patterns);my $inside=0;
while(<$f>){$inside=1,next if /^## Repository path rules/;last if $inside && /^## Shared context/;next unless $inside && /^\| (.*?) \| (.*?) \|/;my($paths,$owner)=($1,$2);next if $paths eq 'Path or directory prefix';for my $p(split /; /,$paths){$p='*' if $p=~/^\* /;die "duplicate pattern $p" if $patterns{$p}++;push @rules,[$p,$owner]}}
die 'missing explicit default' unless $rules[-1][0] eq '*';
sub owner {my($p)=@_;for my $r(@rules){return $r->[1] if $r->[0] eq '*' || ($r->[0]=~m{/$}?index($p,$r->[0])==0:$p eq $r->[0]);}die "unowned $p"}
my(%count,$n);open my $list,'-|','rg','--files','--hidden','-g','!target/**','-g','!.git' or die $!;while(<$list>){chomp;my $o=owner($_);die "invalid owner $o" unless $o=~/^(?:integrate-and-retire-legacy|write-devops-and-user-docs|build-xtask-and-manifest|build-test-support-crate|slice-[a-z0-9-]+|ci-workflows|release-operator-contract|candidate-promote-pipeline)$/;$count{$o}++;$n++}
close $list or die 'file enumeration failed';
# Planned exact paths and domain prefixes must also resolve to their declared owner.
for my $r(@rules){next if $r->[0] eq '*';my $p=$r->[0];$p.='__ownership_probe__' if $p=~m{/$};die "shadowed rule $r->[0]" unless owner($p) eq $r->[1]}
print "PASS: $n enumerated repository paths resolve to exactly one owner.\n";
print "PASS: ".scalar(@rules)." unique rules, explicit default, no shadowed owner rule.\n";
print "$_: $count{$_}\n" for sort keys %count;
