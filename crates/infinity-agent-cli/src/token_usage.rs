/// Construct a streaming response carrying only a total-token count.
pub trait WithTotalTokens {
    fn with_total_tokens(total: usize) -> Self;
}

impl WithTotalTokens for rig_bedrock::streaming::BedrockStreamingResponse {
    fn with_total_tokens(total: usize) -> Self {
        Self {
            usage: Some(rig_bedrock::streaming::BedrockUsage {
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: total as i32,
            }),
        }
    }
}
