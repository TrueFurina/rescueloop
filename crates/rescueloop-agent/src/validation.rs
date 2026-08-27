use rescueloop_core::{AnalysisError, AnalysisRequest, AnalysisResponse};

pub const ALLOWED_ACTIONS: &[&str] = &[
    "quarantine_path",
    "regenerate_cache",
    "patch_json_config",
    "set_permission",
    "restart_service",
    "restart_container",
];

pub fn validate(
    request: &AnalysisRequest,
    response: &AnalysisResponse,
) -> Result<(), AnalysisError> {
    for hypothesis in &response.hypotheses {
        if !(0.0..=1.0).contains(&hypothesis.confidence) {
            return Err(AnalysisError::Invalid(
                "hypothesis confidence must be between 0 and 1".into(),
            ));
        }
        if hypothesis
            .evidence_indexes
            .iter()
            .any(|index| *index >= request.incident.evidence.len())
        {
            return Err(AnalysisError::Invalid(
                "hypothesis references missing evidence".into(),
            ));
        }
    }
    for action in &response.proposed_actions {
        if !ALLOWED_ACTIONS.contains(&action.action_type.as_str()) {
            return Err(AnalysisError::Invalid(format!(
                "action type is not allowed: {}",
                action.action_type
            )));
        }
        if !request.allowed_actions.contains(&action.action_type) {
            return Err(AnalysisError::Invalid(format!(
                "action is unavailable on this platform: {}",
                action.action_type
            )));
        }
        if !action.reversible {
            return Err(AnalysisError::Invalid(format!(
                "non-reversible action rejected: {}",
                action.action_type
            )));
        }
        validate_parameters(&action.action_type, &action.parameters)?;
    }
    Ok(())
}

fn validate_parameters(
    action_type: &str,
    parameters: &serde_json::Value,
) -> Result<(), AnalysisError> {
    let required: &[&str] = match action_type {
        "quarantine_path" | "regenerate_cache" => &["target"],
        "patch_json_config" => &["target", "pointer", "value"],
        "set_permission" => &["target", "mode"],
        "restart_service" => &["service_id"],
        "restart_container" => &["engine", "container_id"],
        _ => return Ok(()),
    };
    let object = parameters.as_object().ok_or_else(|| {
        AnalysisError::Invalid(format!("{action_type} parameters must be an object"))
    })?;
    for key in required {
        if !object.contains_key(*key) {
            return Err(AnalysisError::Invalid(format!(
                "{action_type} is missing parameter: {key}"
            )));
        }
    }
    let require_string = |key: &str| {
        object
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                AnalysisError::Invalid(format!(
                    "{action_type} parameter {key} must be a non-empty string"
                ))
            })
    };
    match action_type {
        "quarantine_path" | "regenerate_cache" | "patch_json_config" | "set_permission" => {
            require_string("target")?;
        }
        "restart_service" => {
            require_string("service_id")?;
        }
        "restart_container" => {
            let engine = require_string("engine")?;
            if !matches!(engine, "docker" | "podman") {
                return Err(AnalysisError::Invalid(
                    "restart_container engine must be docker or podman".into(),
                ));
            }
            require_string("container_id")?;
        }
        _ => {}
    }
    if action_type == "set_permission" {
        let mode = require_string("mode")?.trim_start_matches("0o");
        let parsed = u32::from_str_radix(mode, 8)
            .map_err(|_| AnalysisError::Invalid("permission mode must be octal".into()))?;
        if parsed > 0o7777 {
            return Err(AnalysisError::Invalid(
                "permission mode exceeds supported POSIX bits".into(),
            ));
        }
    }
    if action_type == "patch_json_config" {
        let pointer = require_string("pointer")?;
        if !pointer.starts_with('/') {
            return Err(AnalysisError::Invalid(
                "JSON pointer must start with /".into(),
            ));
        }
    }
    Ok(())
}
