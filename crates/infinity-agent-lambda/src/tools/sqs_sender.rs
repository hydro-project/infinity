use async_trait::async_trait;
use aws_sdk_sqs::Client as SqsClient;
use infinity_agent_core::traits::MessageSender;

#[derive(Clone)]
pub struct SqsMessageSender {
    pub sqs_client: SqsClient,
    pub input_queue_url: String,
    pub output_queue_url: String,
}

#[derive(Debug)]
pub struct SqsError(String);
impl std::fmt::Display for SqsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for SqsError {}

#[async_trait]
impl MessageSender for SqsMessageSender {
    type Error = SqsError;

    async fn send_to_input_queue(
        &self,
        body: &str,
        group_id: &str,
        dedup_id: &str,
    ) -> Result<(), SqsError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        self.sqs_client
            .send_message()
            .queue_url(&self.input_queue_url)
            .message_body(body)
            .message_group_id(group_id)
            .message_deduplication_id(format!("{}-{}", dedup_id, now))
            .send()
            .await
            .map_err(|e| SqsError(format!("Failed to send to input queue: {}", e)))?;
        Ok(())
    }

    async fn send_to_output(&self, body: &str) -> Result<(), SqsError> {
        if self.output_queue_url.is_empty() {
            return Ok(());
        }
        self.sqs_client
            .send_message()
            .queue_url(&self.output_queue_url)
            .message_body(body)
            .send()
            .await
            .map_err(|e| SqsError(format!("Failed to send to output queue: {}", e)))?;
        Ok(())
    }
}
