use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub const MAX_QUESTIONS: usize = 5;
pub const MAX_QUESTION_CHARS: usize = 400;
pub const MAX_OPTIONS: usize = 6;
pub const MAX_OPTION_CHARS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionnaireQuestion {
    pub question: String,
    #[serde(default)]
    pub options: Vec<String>,
    /// 多选：用户可勾选多个选项（默认单选）。
    #[serde(default)]
    pub multiple: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionnaireRequest {
    pub questions: Vec<QuestionnaireQuestion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionnaireResponse {
    pub questionnaire_id: String,
}

pub type QuestionnaireHandler = Arc<
    dyn Fn(
            QuestionnaireRequest,
        ) -> Pin<Box<dyn Future<Output = Result<QuestionnaireResponse, String>> + Send>>
        + Send
        + Sync,
>;

pub fn validate_questions(questions: &mut [QuestionnaireQuestion]) -> Result<(), String> {
    if questions.is_empty() {
        return Err("questionnaire must contain at least one question".to_string());
    }
    if questions.len() > MAX_QUESTIONS {
        return Err(format!(
            "questionnaire must not exceed {MAX_QUESTIONS} questions"
        ));
    }
    for question in questions {
        question.question = question.question.trim().to_string();
        if question.question.is_empty() {
            return Err("question text must not be empty".to_string());
        }
        if question.question.chars().count() > MAX_QUESTION_CHARS {
            return Err(format!(
                "question text must not exceed {MAX_QUESTION_CHARS} characters"
            ));
        }
        if question.options.len() > MAX_OPTIONS {
            return Err(format!("question options must not exceed {MAX_OPTIONS}"));
        }
        for option in &mut question.options {
            *option = option.trim().to_string();
            if option.is_empty() {
                return Err("option text must not be empty".to_string());
            }
            if option.chars().count() > MAX_OPTION_CHARS {
                return Err(format!(
                    "option text must not exceed {MAX_OPTION_CHARS} characters"
                ));
            }
        }
        for (index, option) in question.options.iter().enumerate() {
            if question.options[..index].contains(option) {
                return Err("option text must be unique within a question".to_string());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(text: &str, options: &[&str]) -> QuestionnaireQuestion {
        QuestionnaireQuestion {
            question: text.to_string(),
            options: options.iter().map(|option| option.to_string()).collect(),
            multiple: false,
        }
    }

    #[test]
    fn trims_question_and_option_whitespace() {
        let mut questions = vec![question("  pick one  ", &["  a ", "b"])];
        validate_questions(&mut questions).unwrap();
        assert_eq!(questions[0].question, "pick one");
        assert_eq!(questions[0].options, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn rejects_empty_and_oversized_questionnaires() {
        let mut empty = Vec::new();
        assert_eq!(
            validate_questions(&mut empty).unwrap_err(),
            "questionnaire must contain at least one question"
        );
        let mut too_many = (0..=MAX_QUESTIONS)
            .map(|index| question(&format!("q{index}"), &[]))
            .collect::<Vec<_>>();
        assert!(
            validate_questions(&mut too_many)
                .unwrap_err()
                .contains("must not exceed")
        );

        let mut empty_question = vec![question("   ", &[])];
        assert_eq!(
            validate_questions(&mut empty_question).unwrap_err(),
            "question text must not be empty"
        );

        let mut too_long = vec![question(&"x".repeat(MAX_QUESTION_CHARS + 1), &[])];
        assert!(
            validate_questions(&mut too_long)
                .unwrap_err()
                .contains("characters")
        );
    }

    #[test]
    fn rejects_invalid_options() {
        let mut too_many = vec![question("q", &["1", "2", "3", "4", "5", "6", "7"])];
        assert!(
            validate_questions(&mut too_many)
                .unwrap_err()
                .contains("must not exceed")
        );

        let mut empty_option = vec![question("q", &["ok", "  "])];
        assert_eq!(
            validate_questions(&mut empty_option).unwrap_err(),
            "option text must not be empty"
        );

        let mut duplicates = vec![question("q", &["a", "a"])];
        assert!(
            validate_questions(&mut duplicates)
                .unwrap_err()
                .contains("unique")
        );

        let long = "x".repeat(MAX_OPTION_CHARS + 1);
        let mut too_long = vec![question("q", &[long.as_str()])];
        assert!(
            validate_questions(&mut too_long)
                .unwrap_err()
                .contains("characters")
        );
    }

    #[test]
    fn accepts_and_round_trips_multi_select() {
        let mut questions = vec![QuestionnaireQuestion {
            question: "pick some".to_string(),
            options: vec!["a".to_string(), "b".to_string()],
            multiple: true,
        }];
        validate_questions(&mut questions).unwrap();
        let wire = serde_json::to_value(&questions[0]).unwrap();
        assert_eq!(wire["multiple"], true);
        let back: QuestionnaireQuestion = serde_json::from_value(wire).unwrap();
        assert!(back.multiple);
        let plain: QuestionnaireQuestion =
            serde_json::from_value(serde_json::json!({"question": "q"})).unwrap();
        assert!(!plain.multiple);
    }
}
