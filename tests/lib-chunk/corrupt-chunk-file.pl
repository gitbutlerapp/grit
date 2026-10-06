#!/usr/bin/perl

my ($chunk, $seek, $bytes) = @ARGV;
$bytes =~ s/../chr(hex($&))/ge;

binmode STDIN;
binmode STDOUT;

sub get {
	my $n = shift;
	return unless $n;
	read(STDIN, my $buf, $n)
		or die "read error or eof: $!\n";
	return $buf;
}
sub copy {
	my $buf = get(@_);
	print $buf;
	return $buf;
}

sub unpack_quad {
	my $bytes = shift;
	my ($n1, $n2) = unpack("NN", $bytes);
	die "quad value exceeds 32 bits" if $n1;
	return $n2;
}
sub pack_quad {
	my $n = shift;
	my $ret = pack("NN", 0, $n);
	die "quad round-trip failed" if unpack_quad($ret) != $n;
	return $ret;
}

while (copy(4) ne $chunk) { }
my $offset = unpack_quad(copy(8));

my $len;
if ($seek eq "clear") {
	my $id;
	do {
		$id = copy(4);
		my $next = unpack_quad(get(8));
		if (!defined $len) {
			$len = $next - $offset;
		}
		print pack_quad($next - $len + length($bytes));
	} while (unpack("N", $id));
}

copy($offset - tell(STDIN));
if ($seek eq "clear") {
	get($len);
} else {
	copy($seek);
	get(length($bytes));
}

print $bytes;
while (read(STDIN, my $buf, 4096)) {
	print $buf;
}
