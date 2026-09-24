//! Provider-neutral tool definitions.
//! Converts the Gemini-specific `FunctionDeclaration` format to `ToolDefinition`.
//! The `execute_tool()` function in `gemini_tools.rs` remains unchanged —
//! it's already provider-agnostic (takes `&str` name + `&Value` args).

use super::ToolDefinition;
use crate::gemini_tools;

/// The game-data lookups a Choya question reply may call: every plating
/// tool except the ones that evaluate or search for a build. A question is
/// answered from the game data, not by running the optimizer.
pub fn lookup_tool_definitions() -> Vec<ToolDefinition> {
    const NOT_LOOKUPS: &[&str] = &[
        "simulate_combat",
        "simulate_rotation",
        "score_build",
        "get_optimizer_results",
    ];
    tool_definitions()
        .into_iter()
        .filter(|t| !NOT_LOOKUPS.contains(&t.name.as_str()))
        .collect()
}

/// Build all tool definitions in provider-neutral format.
/// Each provider's `LlmClient` implementation converts these to its wire format.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    // Reuse the existing Gemini declarations and convert
    let gemini_tools = gemini_tools::tool_declarations();
    gemini_tools
        .into_iter()
        .flat_map(|tool| tool.function_declarations)
        .map(|decl| ToolDefinition {
            name: decl.name,
            description: decl.description,
            parameters: decl.parameters,
        })
        .collect()
}
