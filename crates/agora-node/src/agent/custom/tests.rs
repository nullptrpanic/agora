use super::*;

#[derive(Default)]
struct CollectedOutput {
    answer: String,
}

impl AgentOutput for CollectedOutput {
    async fn write(&mut self, event: OutputEvent) -> Result<()> {
        let OutputEvent::Answer { text } = event else {
            panic!("custom command output emitted a non-answer event");
        };
        self.answer.push_str(&text);
        Ok(())
    }
}

#[tokio::test]
async fn raw_command_output_preserves_utf8_split_across_each_stream() {
    let mut output = CollectedOutput::default();
    let mut raw = RawCommandOutput::new(&mut output);

    raw.stdout(&[0xe7]).await.unwrap();
    raw.stderr(&[0xf0, 0x9f]).await.unwrap();
    raw.stdout(&[0x95, 0x8c]).await.unwrap();
    raw.stderr(&[0x99, 0x82]).await.unwrap();
    raw.finish().await.unwrap();

    assert_eq!(output.answer, "界🙂");
}

#[tokio::test]
async fn raw_command_output_flushes_an_invalid_final_utf8_tail_lossily() {
    let mut output = CollectedOutput::default();
    let mut raw = RawCommandOutput::new(&mut output);

    raw.stdout(b"answer\xe7").await.unwrap();
    raw.finish().await.unwrap();

    assert_eq!(output.answer, "answer�");
}
