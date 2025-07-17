fn main() {
	#[cfg(not(feature = "internal_benches"))]
	panic!("benches must have -F internal_benches");
	use agentgateway as _;
	divan::main();
}
