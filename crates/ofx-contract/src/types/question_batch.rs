#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionBatchEntry {
    pub question: String,
    pub options: Vec<QuestionOption>,
}
