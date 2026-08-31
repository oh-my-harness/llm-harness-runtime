//! 内置 predicate：Contains / NumberRangeField / RegexField / RateLimit。

use std::sync::Mutex;

use serde_json::Value;

use crate::rules::predicate::Predicate;

/// 集合包含：tool_name ∈ allowed_tools。
/// P0-A 流程引擎越界兜底就用这个（tool_name 是否在当前步 allowed_tools 里）。
pub struct Contains {
    pub(crate) allowed: Vec<String>,
}

impl Contains {
    pub fn new(allowed: Vec<String>) -> Self {
        Self { allowed }
    }
}

impl Predicate for Contains {
    fn test(&self, tool_name: &str, _args: &Value) -> bool {
        self.allowed.iter().any(|t| t == tool_name)
    }

    fn describe(&self) -> String {
        format!("tool_name in {:?}", self.allowed)
    }
}

/// 从 args 按 arg_path 取值。arg_path 用点号分隔（如 "config.threshold"）。
/// None = 不取参数（如 Contains 只看 tool_name）。
fn extract_arg<'a>(args: &'a Value, arg_path: &str) -> Option<&'a Value> {
    let mut cur = args;
    for seg in arg_path.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

/// 数值区间：args[arg_path] 在 [min, max]。predicate 内部持 arg_path。
pub struct NumberRangeField {
    pub arg_path: String,
    pub min: f64,
    pub max: f64,
}

impl Predicate for NumberRangeField {
    fn test(&self, _tool_name: &str, args: &Value) -> bool {
        let v = match extract_arg(args, &self.arg_path) {
            Some(v) => v,
            None => return false,
        };
        let n = match v.as_f64() {
            Some(n) => n,
            None => return false,
        };
        n >= self.min && n <= self.max
    }

    fn describe(&self) -> String {
        format!("args.{} in [{}, {}]", self.arg_path, self.min, self.max)
    }
}

/// 正则匹配：args[arg_path] 匹配 pattern。
pub struct RegexField {
    pub arg_path: String,
    pub pattern: regex::Regex,
}

impl Predicate for RegexField {
    fn test(&self, _tool_name: &str, args: &Value) -> bool {
        let v = match extract_arg(args, &self.arg_path) {
            Some(v) => v,
            None => return false,
        };
        let s = match v.as_str() {
            Some(s) => s,
            None => return false,
        };
        self.pattern.is_match(s)
    }

    fn describe(&self) -> String {
        format!("args.{} ~ /{}/", self.arg_path, self.pattern.as_str())
    }
}

/// Rate limit：单位时间内同 tool_name 调用次数 ≤ max。
/// 内部用 Mutex<HashMap<tool_name, Vec<Instant>>> 记录调用时间窗。
///
/// 典型用法：on_match: Allow（限流放行）+ fallback: Deny（超限拒绝）。
/// 勿配 on_match: Deny——test() 在返回 true 时记录时间戳，配 Deny 会导致
/// 「前 max 次被拒、之后放行」的语义反转。
pub struct RateLimit {
    pub(crate) max: u32,
    pub(crate) window: std::time::Duration,
    calls: Mutex<std::collections::HashMap<String, Vec<std::time::Instant>>>,
}

impl RateLimit {
    pub fn new(max: u32, window: std::time::Duration) -> Self {
        Self {
            max,
            window,
            calls: Mutex::new(std::collections::HashMap::new()),
        }
    }
}

impl Predicate for RateLimit {
    fn test(&self, tool_name: &str, _args: &Value) -> bool {
        // spec: Mutex poisoned → 返回 false → 走 fallback Deny（fail-secure）
        let mut calls = match self.calls.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        let now = std::time::Instant::now();
        let window = self.window;
        let (should_remove, result) = {
            let timestamps = calls.entry(tool_name.to_string()).or_default();
            // 清理过期
            timestamps.retain(|&t| now.duration_since(t) < window);
            if timestamps.len() >= self.max as usize {
                // 超限：拒绝。若全部过期（len=0 且 max=0），标记可清理。
                (timestamps.is_empty(), false)
            } else {
                timestamps.push(now);
                (false, true)
            }
        };
        if should_remove {
            calls.remove(tool_name);
        }
        result
    }

    fn describe(&self) -> String {
        let secs = self.window.as_secs();
        if secs > 0 {
            format!("rate ≤ {}/{}s", self.max, secs)
        } else {
            format!("rate ≤ {}/{}ms", self.max, self.window.as_millis())
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_matches_member() {
        let c = Contains::new(vec!["tool_a".into(), "tool_b".into()]);
        assert!(c.test("tool_a", &Value::Null));
        assert!(c.test("tool_b", &Value::Null));
    }

    #[test]
    fn contains_rejects_non_member() {
        let c = Contains::new(vec!["tool_a".into()]);
        assert!(!c.test("tool_c", &Value::Null));
    }

    #[test]
    fn contains_empty_rejects_all() {
        let c = Contains::new(vec![]);
        assert!(!c.test("any", &Value::Null));
    }

    #[test]
    fn number_range_in_bounds() {
        let p = NumberRangeField {
            arg_path: "value".into(),
            min: 0.0,
            max: 5.0,
        };
        assert!(p.test("set_voltage", &serde_json::json!({"value": 2.5})));
        assert!(p.test("set_voltage", &serde_json::json!({"value": 0.0})));
        assert!(p.test("set_voltage", &serde_json::json!({"value": 5.0})));
    }

    #[test]
    fn number_range_out_of_bounds() {
        let p = NumberRangeField {
            arg_path: "value".into(),
            min: 0.0,
            max: 5.0,
        };
        assert!(!p.test("set_voltage", &serde_json::json!({"value": 5.1})));
        assert!(!p.test("set_voltage", &serde_json::json!({"value": -0.1})));
    }

    #[test]
    fn number_range_missing_path() {
        let p = NumberRangeField {
            arg_path: "value".into(),
            min: 0.0,
            max: 5.0,
        };
        assert!(!p.test("set_voltage", &serde_json::json!({"other": 1.0})));
    }

    #[test]
    fn number_range_nested_path() {
        let p = NumberRangeField {
            arg_path: "config.threshold".into(),
            min: 0.0,
            max: 10.0,
        };
        assert!(p.test("x", &serde_json::json!({"config": {"threshold": 7.0}})));
        assert!(!p.test("x", &serde_json::json!({"config": {"threshold": 11.0}})));
    }

    #[test]
    fn regex_matches() {
        let p = RegexField {
            arg_path: "path".into(),
            pattern: regex::Regex::new(r"^/tmp/").unwrap(),
        };
        assert!(p.test("delete", &serde_json::json!({"path": "/tmp/foo"})));
        assert!(!p.test("delete", &serde_json::json!({"path": "/etc/passwd"})));
    }

    #[test]
    fn rate_limit_allows_under_max() {
        let p = RateLimit::new(3, std::time::Duration::from_secs(60));
        assert!(p.test("tool", &Value::Null));
        assert!(p.test("tool", &Value::Null));
        assert!(p.test("tool", &Value::Null));
    }

    #[test]
    fn rate_limit_blocks_over_max() {
        let p = RateLimit::new(2, std::time::Duration::from_secs(60));
        assert!(p.test("tool", &Value::Null));
        assert!(p.test("tool", &Value::Null));
        assert!(!p.test("tool", &Value::Null)); // 第 3 次被拒
    }

    #[test]
    fn rate_limit_independent_per_tool() {
        let p = RateLimit::new(1, std::time::Duration::from_secs(60));
        assert!(p.test("a", &Value::Null));
        assert!(!p.test("a", &Value::Null));
        assert!(p.test("b", &Value::Null)); // 不同 tool 独立计数
    }
}
