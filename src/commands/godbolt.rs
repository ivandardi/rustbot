use std::{collections::HashMap, mem::take};

use anyhow::{anyhow, Error};
use poise::{CodeBlockError, KeyValueArgs};
use tracing::warn;

use crate::types::Context;

mod targets;
pub use targets::*;

const LLVM_MCA_TOOL_ID: &str = "llvm-mcatrunk";

struct Compilation {
	output: String,
	stderr: String,
}

#[derive(Debug, serde::Deserialize)]
struct GodboltOutputSegment {
	text: String,
}

#[derive(Debug, serde::Deserialize)]
struct GodboltOutput(Vec<GodboltOutputSegment>);

impl GodboltOutput {
	pub fn concatenate(&self) -> String {
		let mut complete_text = String::new();
		for segment in &self.0 {
			complete_text.push_str(&segment.text);
			complete_text.push('\n');
		}
		complete_text
	}
}

#[derive(Debug, serde::Deserialize)]
struct GodboltResponse {
	// stdout: GodboltOutput,
	stderr: GodboltOutput,
	asm: GodboltOutput,
	tools: Vec<GodboltTool>,
}

#[derive(Debug, serde::Deserialize)]
struct GodboltTool {
	id: String,
	// code: u8,
	stdout: GodboltOutput,
	// stderr: GodboltOutput,
}

struct GodboltRequest<'a> {
	source_code: &'a str,
	rustc: &'a str,
	flags: &'a str,
	run_llvm_mca: bool,
}

/// Compile a given Rust source code file on Godbolt using the latest nightly compiler with
/// full optimizations (-O3)
/// Returns a multiline string with the pretty printed assembly
async fn compile_rust_source(
	http: &reqwest::Client,
	request: &GodboltRequest<'_>,
) -> Result<Compilation, Error> {
	let tools = if request.run_llvm_mca {
		serde_json::json! {
			[{"id": LLVM_MCA_TOOL_ID}]
		}
	} else {
		serde_json::json! {
			[]
		}
	};

	let http_request = http
		.post(format!(
			"https://godbolt.org/api/compiler/{}/compile",
			request.rustc
		))
		.header(reqwest::header::ACCEPT, "application/json") // to make godbolt respond in JSON
		.json(&serde_json::json! { {
            "source": request.source_code,
            "options": {
                "userArguments": request.flags,
                "tools": tools,
                // "libraries": [{"id": "itoa", "version": "102"}],
            },
        } })
		.build()?;

	let response: GodboltResponse = http.execute(http_request).await?.json().await?;

	// TODO: use the extract_relevant_lines utility to strip stderr nicely
	Ok(Compilation {
		output: if request.run_llvm_mca {
			let text = response
				.tools
				.iter()
				.find(|tool| tool.id == LLVM_MCA_TOOL_ID)
				.map(|llvm_mca| llvm_mca.stdout.concatenate())
				.ok_or(anyhow!("No llvm-mca result was sent by Godbolt"))?;
			// Strip junk
			text[..text.find("Instruction Info").unwrap_or(text.len())]
				.trim()
				.to_string()
		} else {
			response.asm.concatenate()
		},
		stderr: response.stderr.concatenate(),
	})
}

async fn save_to_shortlink(http: &reqwest::Client, req: &GodboltRequest<'_>) -> String {
	#[derive(serde::Deserialize)]
	struct GodboltShortenerResponse {
		url: String,
	}

	let tools = if req.run_llvm_mca {
		serde_json::json! {
			[{"id": LLVM_MCA_TOOL_ID}]
		}
	} else {
		serde_json::json! {
			[]
		}
	};

	let request = http
		.post("https://godbolt.org/api/shortener")
		.json(&serde_json::json! { {
			"sessions": [{
				"language": "rust",
				"source": req.source_code,
				"compilers": [{
					"id": req.rustc,
					"options": req.flags,
					"tools": tools,
				}],
			}]
		} });

	// Try block substitute
	let url = async move {
		Ok::<_, crate::Error>(
			request
				.send()
				.await?
				.json::<GodboltShortenerResponse>()
				.await?
				.url,
		)
	};
	url.await.unwrap_or_else(|e| {
		warn!("failed to generate godbolt shortlink: {}", e);
		"failed to retrieve".to_owned()
	})
}

#[derive(PartialEq, Clone, Copy)]
#[allow(unused)]
enum GodboltMode {
	Asm,
	LlvmIr,
	Mca,
}

fn note(code: &str) -> &'static str {
	if code.contains("#[no_mangle]") {
		""
	} else {
		"Note: only unmangled functions (`#[no_mangle] pub fn`) are shown"
	}
}

async fn respond_codeblocks(
	ctx: Context<'_>,
	godbolt_result: Compilation,
	godbolt_request: GodboltRequest<'_>,
	lang: &'static str,
	note: &str,
) -> Result<(), Error> {
	const NO_OUTPUT: &str = "No output. Consider adding `#[no_mangle]` before your functions.";
	match (godbolt_result.output.trim(), godbolt_result.stderr.trim()) {
		("", "") => respond_codeblock(ctx, "", " ", NO_OUTPUT, &godbolt_request).await?,
		(output, "") => respond_codeblock(ctx, lang, output, note, &godbolt_request).await?,
		("<Compilation failed>", errors) => {
			respond_codeblock(ctx, "ansi", errors, "Compilation failed.", &godbolt_request).await?;
		}
		("", warnings) => {
			respond_codeblock(ctx, "ansi", warnings, NO_OUTPUT, &godbolt_request).await?;
		}
		(output, errors) => {
			ctx.say(
				crate::helpers::trim_text(
					&format!("```{lang}\n{output}``````ansi\n{errors}"),
					&format!("\n```{note}"),
					async {
						format!(
							"Output too large. Godbolt link: <{}>",
							save_to_shortlink(&ctx.data().http, &godbolt_request).await,
						)
					},
				)
				.await,
			)
			.await?;
		}
	};
	Ok(())
}

async fn respond_codeblock(
	ctx: Context<'_>,
	codeblock_lang: &str,
	text: &str,
	note: &str,
	godbolt_request: &GodboltRequest<'_>,
) -> Result<(), Error> {
	ctx.say(
		crate::helpers::trim_text(
			&format!("```{codeblock_lang}\n{text}"),
			&format!("\n```{note}"),
			async {
				format!(
					"Output too large. Godbolt link: <{}>",
					save_to_shortlink(&ctx.data().http, godbolt_request).await,
				)
			},
		)
		.await,
	)
	.await?;
	Ok(())
}

fn parse(args: &str) -> Result<(KeyValueArgs, String), CodeBlockError> {
	let mut map = HashMap::new();
	let mut key = String::new();
	let mut value = String::new();
	// flag for in a key
	let mut k = true;
	let mut args = args.chars();
	let mut tick_count = 0;
	// note: you cant put a backtick in an argument
	for ch in args.by_ref() {
		match ch {
			'`' => {
				tick_count += 1;
				break;
			}
			' ' => {
				map.insert(take(&mut key), take(&mut value));
				k = true
			}
			'=' => k = false,
			c if k => key.push(c),
			c => value.push(c),
		}
	}

	// note: language can be parsed, but is discarded here
	let mut parsed_lang = false;
	let mut code = String::new();
	for ch in args {
		match ch {
			// ```
			'`' if tick_count == 3 && !parsed_lang => return Err(CodeBlockError::default()),
			// closing
			'`' if tick_count == 3 && parsed_lang => break,
			'`' => tick_count += 1,
			// ```rust
			//    ^^^^
			'\n' if tick_count == 3 && !parsed_lang => parsed_lang = true,
			_ if tick_count == 3 && !parsed_lang => {}

			c => code.push(c),
		}
	}
	Ok((KeyValueArgs(map), code))
}

/// View assembly using Godbolt
///
/// Compile Rust code using <https://rust.godbolt.org>. Full optimizations are applied unless \
/// overriden.
/// ```
/// ?godbolt flag={} rustc={} ``​`
/// pub fn your_function() {
///     // Code
/// }
/// ``​`
/// ```
/// Optional arguments:
/// - `flag*`: flags to pass to rustc invocation. Defaults to {"-Copt-level=3", "--edition=2024"}
/// - `rustc`: compiler version to invoke. Defaults to `nightly`. Possible values: `nightly`, `beta` or full version like `1.45.2`
#[poise::command(prefix_command, category = "Godbolt", broadcast_typing, track_edits)]
pub async fn godbolt(ctx: Context<'_>, #[rest] arguments: String) -> Result<(), Error> {
	let (params, code) = parse(&arguments)?;
	let (rustc, flags) = rustc_id_and_flags(ctx.data(), &params).await?;
	let godbolt_request = GodboltRequest {
		source_code: &code,
		rustc: &rustc,
		flags: &flags,
		run_llvm_mca: false,
	};
	let godbolt_result = compile_rust_source(&ctx.data().http, &godbolt_request).await?;

	let note = note(&code);
	respond_codeblocks(ctx, godbolt_result, godbolt_request, "x86asm", note).await
}

/// Run performance analysis using llvm-mca
///
/// Run the performance analysis tool llvm-mca using <https://rust.godbolt.org>. Full optimizations \
/// are applied unless overriden.
/// ```
/// ?mca flag={} rustc={} ``​`
/// pub fn your_function() {
///     // Code
/// }
/// ``​`
/// ```
/// Optional arguments:
/// - `flag*`: flags to pass to rustc invocation. Defaults to {"-Copt-level=3", "--edition=2024"}
/// - `rustc`: compiler version to invoke. Defaults to `nightly`. Possible values: `nightly`, `beta` or full version like `1.45.2`
#[poise::command(prefix_command, category = "Godbolt", broadcast_typing, track_edits)]
pub async fn mca(ctx: Context<'_>, #[rest] arguments: String) -> Result<(), Error> {
	let (params, code) = parse(&arguments)?;
	let (rustc, flags) = rustc_id_and_flags(ctx.data(), &params).await?;
	let godbolt_request = GodboltRequest {
		source_code: &code,
		rustc: &rustc,
		flags: &flags,
		run_llvm_mca: true,
	};

	let godbolt_result = compile_rust_source(&ctx.data().http, &godbolt_request).await?;

	let note = note(&code);
	respond_codeblocks(ctx, godbolt_result, godbolt_request, "rust", note).await
}

/// View LLVM IR using Godbolt
///
/// Compile Rust code using <https://rust.godbolt.org> and emits LLVM IR. Full optimizations \
/// are applied unless overriden.
///
/// Equivalent to ?godbolt but with extra flags `--emit=llvm-ir -Cdebuginfo=0`.
/// ```
/// ?llvmir flag={} rustc={} ``​`
/// pub fn your_function() {
///     // Code
/// }
/// ``​`
/// ```
/// Optional arguments:
/// - `flag*`: flags to pass to rustc invocation. Defaults to {"-Copt-level=3", "--edition=2024", "--emit=llvm-ir", "-Cdebuginfo=0"}
/// - `rustc`: compiler version to invoke. Defaults to `nightly`. Possible values: `nightly`, `beta` or full version like `1.45.2`
#[poise::command(prefix_command, category = "Godbolt", broadcast_typing, track_edits)]
pub async fn llvmir(ctx: Context<'_>, #[rest] arguments: String) -> Result<(), Error> {
	let (params, code) = parse(&arguments)?;
	let (rustc, flags) = rustc_id_and_flags(ctx.data(), &params).await?;
	let godbolt_request = GodboltRequest {
		source_code: &code,
		rustc: &rustc,
		flags: &(flags + " --emit=llvm-ir -Cdebuginfo=0"),
		run_llvm_mca: false,
	};
	let godbolt_result = compile_rust_source(&ctx.data().http, &godbolt_request).await?;

	let note = note(&code);
	respond_codeblocks(ctx, godbolt_result, godbolt_request, "llvm", note).await
}
