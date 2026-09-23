//! Provider drivers. v0.1 ships the OpenAI-compatible chat-completions driver,
//! which covers Kimi (Moonshot), OpenAI, DeepSeek, OpenRouter, Groq, Ollama,
//! llama.cpp and vLLM. Responses-API retained reasoning and Anthropic-native
//! drivers plug into the same `Provider` trait.

pub mod openai_compat;

pub use openai_compat::OpenAiCompatProvider;
