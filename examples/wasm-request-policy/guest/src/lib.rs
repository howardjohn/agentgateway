use wasip3 as _;

#[link(wasm_import_module = "agentgateway:policy/host")]
unsafe extern "C" {
	fn http_get(url_ptr: *const u8, url_len: usize, out_ptr: *mut u8, out_cap: usize) -> i32;
	fn cel_eval_bool(expr_ptr: *const u8, expr_len: usize) -> i32;
}

#[unsafe(no_mangle)]
pub extern "C" fn policy_apply() -> i32 {
	if !cel(r#"request.headers["x-user"] == "admin""#) {
		return 1;
	}

	let mut out = [0u8; 128];
	let n = get("http://127.0.0.1:8081/allow", &mut out);
	if n < 0 {
		return 1;
	}

	if &out[..n as usize] == b"allow" { 0 } else { 1 }
}

fn cel(expr: &str) -> bool {
	unsafe { cel_eval_bool(expr.as_ptr(), expr.len()) == 1 }
}

fn get(url: &str, out: &mut [u8]) -> i32 {
	unsafe { http_get(url.as_ptr(), url.len(), out.as_mut_ptr(), out.len()) }
}
