fn main() -> Result<(), anyhow::Error> {
	let cwd = std::env::current_dir()?;
	let proto = cwd.join("proto/vllm_grpc.proto");
	let includes = [cwd.join("proto")];
	let fds = protox::compile([&proto], &includes)?;

	tonic_prost_build::configure()
		.build_client(true)
		.build_server(false)
		.compile_fds_with_config(fds, prost_build::Config::new())?;

	println!("cargo:rerun-if-changed={}", proto.display());
	Ok(())
}
