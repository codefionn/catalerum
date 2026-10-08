//! Validation for native structured decision questions and answers.
//!
//! Keep instructions, criteria, and answer metadata as JSON. Decision models
//! accept structured instructions and return typed verdicts.

use serde_json::{Map, Value};

fn structured(value: &Value) -> bool {
    matches!(value, Value::String(_) | Value::Object(_) | Value::Array(_))
}

/// Validate named questions before saving an automation or calling a provider.
/// Templates in instructions and criteria remain strings until execution.
pub fn validate_questions(questions: &Map<String, Value>) -> Result<(), String> {
    if questions.is_empty() {
        return Err("questions must be a non-empty object".into());
    }
    for (name, question) in questions {
        if name.trim().is_empty() {
            return Err("questions must have non-empty names".into());
        }
        let question = question
            .as_object()
            .ok_or_else(|| format!("questions.{name} must be an object"))?;
        if !question.get("instructions").is_some_and(structured) {
            return Err(format!(
                "questions.{name}.instructions must be a string, object, or array"
            ));
        }
        match question.get("type").and_then(Value::as_str) {
            Some("noul") => {
                if let Some(criteria) = question.get("criteria") {
                    let criteria = criteria
                        .as_object()
                        .ok_or_else(|| format!("questions.{name}.criteria must be an object"))?;
                    if criteria.values().any(|value| !structured(value)) {
                        return Err(format!(
                            "questions.{name}.criteria values must be strings, objects, or arrays"
                        ));
                    }
                }
            }
            Some("choice") => {
                let criteria = question
                    .get("criteria")
                    .and_then(Value::as_object)
                    .filter(|criteria| !criteria.is_empty())
                    .ok_or_else(|| {
                        format!("questions.{name}.criteria must be a non-empty object")
                    })?;
                if criteria.keys().any(|key| key.trim().is_empty())
                    || criteria
                        .values()
                        .any(|value| !value.is_null() && !structured(value))
                {
                    return Err(format!("questions.{name}.criteria needs named options with null, string, object, or array values"));
                }
            }
            Some("score") => {
                if !question
                    .get("criteria")
                    .and_then(Value::as_array)
                    .is_some_and(|criteria| !criteria.is_empty() && criteria.iter().all(structured))
                {
                    return Err(format!("questions.{name}.criteria must be a non-empty array of strings, objects, or arrays"));
                }
            }
            _ => {
                return Err(format!(
                    "questions.{name}.type must be noul, choice, or score"
                ))
            }
        }
    }
    Ok(())
}

fn number_in_range(value: Option<&Value>, min: f64, max: f64, path: &str) -> Result<(), String> {
    match value.and_then(Value::as_f64) {
        Some(number) if number.is_finite() && (min..=max).contains(&number) => Ok(()),
        _ => Err(format!("{path} must be a number between {min} and {max}")),
    }
}

/// Reject missing, malformed, or out-of-range answers without altering metadata.
/// Score criteria describe positions numbered from zero in the provider scale.
pub fn validate_answers(
    questions: &Map<String, Value>,
    answers: &Map<String, Value>,
) -> Result<(), String> {
    validate_questions(questions)?;
    for (name, question) in questions {
        let answer = answers
            .get(name)
            .and_then(Value::as_object)
            .ok_or_else(|| format!("answers.{name} is missing or must be an object"))?;
        let kind = question["type"].as_str().expect("validated question type");
        if answer.get("type").and_then(Value::as_str) != Some(kind) {
            return Err(format!("answers.{name}.type must be {kind}"));
        }
        match kind {
            "noul" => number_in_range(
                answer.get("noul"),
                0.0,
                1.0,
                &format!("answers.{name}.noul"),
            )?,
            "choice" => {
                let criteria = question["criteria"]
                    .as_object()
                    .expect("validated criteria");
                if !answer
                    .get("choice")
                    .and_then(Value::as_str)
                    .is_some_and(|choice| criteria.contains_key(choice))
                {
                    return Err(format!(
                        "answers.{name}.choice must name a requested option"
                    ));
                }
            }
            "score" => {
                let criteria = question["criteria"].as_array().expect("validated criteria");
                number_in_range(
                    answer.get("score"),
                    0.0,
                    (criteria.len() - 1) as f64,
                    &format!("answers.{name}.score"),
                )?;
                if answer
                    .get("legend")
                    .is_some_and(|legend| !legend.is_object())
                {
                    return Err(format!("answers.{name}.legend must be an object"));
                }
            }
            _ => unreachable!("validated question type"),
        }
        if answer.contains_key("confidence") {
            number_in_range(
                answer.get("confidence"),
                0.0,
                1.0,
                &format!("answers.{name}.confidence"),
            )?;
        }
        if let Some(probabilities) = answer.get("probabilities") {
            let probabilities = probabilities
                .as_object()
                .ok_or_else(|| format!("answers.{name}.probabilities must be an object"))?;
            for (option, probability) in probabilities {
                number_in_range(
                    Some(probability),
                    0.0,
                    1.0,
                    &format!("answers.{name}.probabilities.{option}"),
                )?;
                if kind == "choice"
                    && !question["criteria"]
                        .as_object()
                        .expect("validated criteria")
                        .contains_key(option)
                {
                    return Err(format!(
                        "answers.{name}.probabilities.{option} is not a requested option"
                    ));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn questions() -> Map<String, Value> {
        json!({
            "urgent": {"type":"noul", "instructions":{"question":"Urgent?"}, "criteria":{"true":["blocked"]}},
            "team": {"type":"choice", "instructions":"Route {{ trigger.text }}", "criteria":{"support":null,"engineering":{"meaning":"bugs"}}},
            "severity": {"type":"score", "instructions":["Rate severity"], "criteria":["low",{"label":"medium"},["high"]]}
        }).as_object().unwrap().clone()
    }

    #[test]
    fn accepts_all_question_types_and_structured_templates() {
        assert!(validate_questions(&questions()).is_ok());
    }

    #[test]
    fn rejects_invalid_question_shapes() {
        for value in [
            json!({}),
            json!({" ":{"type":"noul","instructions":"x"}}),
            json!({"x":{"type":"other","instructions":"x"}}),
            json!({"x":{"type":"noul","instructions":null}}),
            json!({"x":{"type":"noul","instructions":"x","criteria":{"true":true}}}),
            json!({"x":{"type":"choice","instructions":"x","criteria":{}}}),
            json!({"x":{"type":"choice","instructions":"x","criteria":{"a":4}}}),
            json!({"x":{"type":"score","instructions":"x","criteria":[1]}}),
        ] {
            assert!(
                validate_questions(value.as_object().unwrap()).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn validates_verdicts_without_discarding_metadata() {
        let answers = json!({
            "urgent":{"type":"noul","noul":0.95},
            "team":{"type":"choice","choice":"support","confidence":0.8,"probabilities":{"support":0.8,"engineering":0.2}},
            "severity":{"type":"score","score":1.8,"legend":{"0":"low","2":"high"},"probabilities":{"0":0.1,"2":0.9}}
        }).as_object().unwrap().clone();
        assert!(validate_answers(&questions(), &answers).is_ok());
        for (name, field, bad) in [
            ("urgent", "noul", json!(1.1)),
            ("urgent", "noul", json!("yes")),
            ("urgent", "type", json!("choice")),
            ("team", "choice", json!("billing")),
            ("team", "confidence", json!(-0.1)),
            ("team", "probabilities", json!({"support":2})),
            ("severity", "score", json!(3)),
            ("severity", "legend", json!(["low"])),
        ] {
            let mut invalid = answers.clone();
            invalid.get_mut(name).unwrap()[field] = bad;
            assert!(
                validate_answers(&questions(), &invalid).is_err(),
                "{invalid:?}"
            );
        }
        let mut missing = answers;
        missing.remove("urgent");
        assert!(validate_answers(&questions(), &missing).is_err());
    }
}
