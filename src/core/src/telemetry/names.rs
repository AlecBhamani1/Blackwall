//! Metric and attribute names. Standard names come from the OpenTelemetry semantic conventions
//! crate; `blackwall.*` is our namespace for what the conventions do not define yet.
// opentelemetry-semantic-conventions 0.33.1 still ships the GenAI names but marks them
// deprecated (they moved to the GenAI conventions repository), so they are repeated here and a
// test pins each one to the crate's value.
pub const ERROR_TYPE: &str = "error.type";
pub const GEN_AI_AGENT_ID: &str = "gen_ai.agent.id";
pub const GEN_AI_AGENT_NAME: &str = "gen_ai.agent.name";
pub const GEN_AI_INPUT_MESSAGES: &str = "gen_ai.input.messages";
pub const GEN_AI_OPERATION_NAME: &str = "gen_ai.operation.name";
pub const GEN_AI_OUTPUT_MESSAGES: &str = "gen_ai.output.messages";
pub const GEN_AI_PROVIDER_NAME: &str = "gen_ai.provider.name";
pub const GEN_AI_REQUEST_MODEL: &str = "gen_ai.request.model";
pub const GEN_AI_RESPONSE_FINISH_REASONS: &str = "gen_ai.response.finish_reasons";
pub const GEN_AI_SYSTEM_INSTRUCTIONS: &str = "gen_ai.system_instructions";
pub const GEN_AI_TOKEN_TYPE: &str = "gen_ai.token.type";
pub const GEN_AI_TOOL_CALL_ARGUMENTS: &str = "gen_ai.tool.call.arguments";
pub const GEN_AI_TOOL_CALL_ID: &str = "gen_ai.tool.call.id";
pub const GEN_AI_TOOL_CALL_RESULT: &str = "gen_ai.tool.call.result";
pub const GEN_AI_TOOL_DEFINITIONS: &str = "gen_ai.tool.definitions";
pub const GEN_AI_TOOL_NAME: &str = "gen_ai.tool.name";
pub const GEN_AI_TOOL_TYPE: &str = "gen_ai.tool.type";
pub const GEN_AI_USAGE_INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
pub const GEN_AI_USAGE_OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";
pub const SERVER_ADDRESS: &str = "server.address";
pub const SERVICE_NAME: &str = "service.name";
pub const SERVICE_VERSION: &str = "service.version";
pub const GEN_AI_CLIENT_OPERATION_DURATION: &str = "gen_ai.client.operation.duration";
pub const GEN_AI_CLIENT_TOKEN_USAGE: &str = "gen_ai.client.token.usage";
/// The semantic-conventions version recorded on every resource and scope.
pub use opentelemetry_semantic_conventions::SCHEMA_URL;

pub const TOOL_INVOCATIONS: &str = "blackwall.tool.invocations";
pub const RUN_OUTCOMES: &str = "blackwall.run.outcomes";
/// Output tokens per second of each model call that produced output.
pub const OUTPUT_SPEED: &str = "blackwall.client.output_speed";
/// Estimated tokens each compaction removed from the context.
pub const COMPACTION_SAVED: &str = "blackwall.compaction.tokens_saved";
/// Finished runs whose trace was dropped because the writer queue was full.
pub const DROPPED_RUNS: &str = "blackwall.trace.dropped_runs";

pub const MODE: &str = "blackwall.mode";
/// The workspace folder's name (never its path). Agent runs only; Chat has no project.
pub const PROJECT: &str = "blackwall.project";
/// The neutral conversation ID a run belongs to; never a title or prompt.
pub const SESSION: &str = "blackwall.session";
pub const SURFACE: &str = "blackwall.surface";
pub const REQUEST_ID: &str = "blackwall.request_id";
pub const TOOL_OUTCOME: &str = "blackwall.tool.outcome";
pub const RUN_OUTCOME: &str = "blackwall.run.outcome";
pub const APPROVAL_KIND: &str = "blackwall.approval.kind";
/// Marks a model call that summarized history for compaction.
pub const COMPACTION: &str = "blackwall.compaction";
pub const DROPPED_SPANS: &str = "blackwall.trace.dropped_spans";
pub const CONTENT_REMOVED: &str = "blackwall.trace.content_removed";

/// Attributes that hold prompts, outputs, or tool data. Kept only with content capture on, and
/// then redacted, scrubbed, and dropped first when a run is too large.
pub const CONTENT_KEYS: [&str; 6] = [
    GEN_AI_TOOL_CALL_ARGUMENTS,
    GEN_AI_TOOL_CALL_RESULT,
    GEN_AI_OUTPUT_MESSAGES,
    GEN_AI_INPUT_MESSAGES,
    GEN_AI_SYSTEM_INSTRUCTIONS,
    GEN_AI_TOOL_DEFINITIONS,
];

/// With content capture off, only these span and event attributes leave the processor. Anything
/// else, including any content key above, reasoning text, and the model-written
/// `gen_ai.tool.call.id`, is dropped.
pub const METADATA_KEYS: [&str; 16] = [
    GEN_AI_OPERATION_NAME,
    GEN_AI_AGENT_NAME,
    GEN_AI_AGENT_ID,
    GEN_AI_REQUEST_MODEL,
    GEN_AI_PROVIDER_NAME,
    GEN_AI_RESPONSE_FINISH_REASONS,
    GEN_AI_USAGE_INPUT_TOKENS,
    GEN_AI_USAGE_OUTPUT_TOKENS,
    GEN_AI_TOOL_NAME,
    GEN_AI_TOOL_TYPE,
    SERVER_ADDRESS,
    ERROR_TYPE,
    REQUEST_ID,
    MODE,
    APPROVAL_KIND,
    COMPACTION,
];

#[cfg(test)]
mod tests {
    #[test]
    #[allow(deprecated)]
    fn names_match_the_semantic_conventions_crate() {
        use opentelemetry_semantic_conventions::{attribute, metric};
        assert_eq!(super::ERROR_TYPE, attribute::ERROR_TYPE);
        assert_eq!(super::GEN_AI_AGENT_ID, attribute::GEN_AI_AGENT_ID);
        assert_eq!(super::GEN_AI_AGENT_NAME, attribute::GEN_AI_AGENT_NAME);
        assert_eq!(
            super::GEN_AI_INPUT_MESSAGES,
            attribute::GEN_AI_INPUT_MESSAGES
        );
        assert_eq!(
            super::GEN_AI_OPERATION_NAME,
            attribute::GEN_AI_OPERATION_NAME
        );
        assert_eq!(
            super::GEN_AI_OUTPUT_MESSAGES,
            attribute::GEN_AI_OUTPUT_MESSAGES
        );
        assert_eq!(super::GEN_AI_PROVIDER_NAME, attribute::GEN_AI_PROVIDER_NAME);
        assert_eq!(super::GEN_AI_REQUEST_MODEL, attribute::GEN_AI_REQUEST_MODEL);
        assert_eq!(
            super::GEN_AI_RESPONSE_FINISH_REASONS,
            attribute::GEN_AI_RESPONSE_FINISH_REASONS
        );
        assert_eq!(
            super::GEN_AI_SYSTEM_INSTRUCTIONS,
            attribute::GEN_AI_SYSTEM_INSTRUCTIONS
        );
        assert_eq!(super::GEN_AI_TOKEN_TYPE, attribute::GEN_AI_TOKEN_TYPE);
        assert_eq!(
            super::GEN_AI_TOOL_CALL_ARGUMENTS,
            attribute::GEN_AI_TOOL_CALL_ARGUMENTS
        );
        assert_eq!(super::GEN_AI_TOOL_CALL_ID, attribute::GEN_AI_TOOL_CALL_ID);
        assert_eq!(
            super::GEN_AI_TOOL_CALL_RESULT,
            attribute::GEN_AI_TOOL_CALL_RESULT
        );
        assert_eq!(
            super::GEN_AI_TOOL_DEFINITIONS,
            attribute::GEN_AI_TOOL_DEFINITIONS
        );
        assert_eq!(super::GEN_AI_TOOL_NAME, attribute::GEN_AI_TOOL_NAME);
        assert_eq!(super::GEN_AI_TOOL_TYPE, attribute::GEN_AI_TOOL_TYPE);
        assert_eq!(
            super::GEN_AI_USAGE_INPUT_TOKENS,
            attribute::GEN_AI_USAGE_INPUT_TOKENS
        );
        assert_eq!(
            super::GEN_AI_USAGE_OUTPUT_TOKENS,
            attribute::GEN_AI_USAGE_OUTPUT_TOKENS
        );
        assert_eq!(super::SERVER_ADDRESS, attribute::SERVER_ADDRESS);
        assert_eq!(super::SERVICE_NAME, attribute::SERVICE_NAME);
        assert_eq!(super::SERVICE_VERSION, attribute::SERVICE_VERSION);
        assert_eq!(
            super::GEN_AI_CLIENT_OPERATION_DURATION,
            metric::GEN_AI_CLIENT_OPERATION_DURATION
        );
        assert_eq!(
            super::GEN_AI_CLIENT_TOKEN_USAGE,
            metric::GEN_AI_CLIENT_TOKEN_USAGE
        );
    }
}
