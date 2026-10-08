use tiktoken::CoreBpe;

use crate::SimpleChatCompletionMessage;

pub fn num_tokens_from_messages(messages: &[SimpleChatCompletionMessage]) -> u64 {
	let bpe = o200k_base();
	let tokens_per_message = 3;

	let mut num_tokens: u64 = 0;
	for message in messages {
		num_tokens += tokens_per_message;
		num_tokens += 1;
		num_tokens += bpe.count_with_special_tokens(message.content.as_str()) as u64;
	}
	num_tokens += 3;
	num_tokens
}

pub fn preload_tokenizers() {
	let _ = o200k_base();
}

fn o200k_base() -> &'static CoreBpe {
	tiktoken::get_encoding("o200k_base").expect("o200k_base is enabled")
}
