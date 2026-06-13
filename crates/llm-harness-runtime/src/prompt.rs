use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use futures::future::BoxFuture;
use llm_harness::PromptTemplate;

use crate::error::Result;

/// Argument definition for a prompt template parameter.
#[derive(Debug, Clone)]
pub struct ArgumentDef {
    /// Parameter name (used as placeholder key `{{name}}`).
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Whether the argument is required.
    pub required: bool,
}

/// Raw template produced by a `PromptSource`.
/// Uses `{{param_name}}` named placeholder syntax.
#[derive(Debug, Clone)]
pub struct PromptSourceTemplate {
    /// Template name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Template body with `{{param_name}}` named placeholders.
    pub template: String,
    /// Parameter definitions — order determines positional mapping
    /// when compiled to core `PromptTemplate` (`$1`, `$2`, ...).
    pub arguments: Vec<ArgumentDef>,
}

/// Source of prompt templates (MCP prompts, filesystem, etc.).
pub trait PromptSource: Send + Sync {
    /// Discover available templates.
    fn discover(&self) -> BoxFuture<'_, Result<Vec<PromptSourceTemplate>>>;
}

/// Compiles `PromptSourceTemplate` into core `PromptTemplate` by mapping
/// `{{param_name}}` named placeholders to positional `$1`, `$2`, ...
/// based on `arguments` declaration order.
pub struct PromptTemplateCompiler;

impl PromptTemplateCompiler {
    /// Compile a `PromptSourceTemplate` into a core `PromptTemplate`.
    ///
    /// Example:
    ///   arguments = [ArgumentDef { name: "lang" }, ArgumentDef { name: "code" }]
    ///   `"Review {{code}} in {{lang}}"` → `"Review $2 in $1"`
    pub fn compile(
        source: &PromptSourceTemplate,
    ) -> std::result::Result<PromptTemplate, CompileError> {
        let mut template_str = source.template.clone();
        for (idx, arg) in source.arguments.iter().enumerate() {
            let placeholder = format!("{{{{{}}}}}", arg.name);
            let position = format!("${}", idx + 1);
            if !template_str.contains(&placeholder) {
                return Err(CompileError::MissingPlaceholder {
                    template: source.name.clone(),
                    param: arg.name.clone(),
                });
            }
            template_str = template_str.replace(&placeholder, &position);
        }
        // Check for any remaining unmatched {{...}} placeholders.
        if template_str.contains("{{") && template_str.contains("}}") {
            return Err(CompileError::UnmatchedPlaceholders {
                template: source.name.clone(),
                remaining: template_str.clone(),
            });
        }
        Ok(PromptTemplate {
            name: source.name.clone(),
            description: source.description.clone(),
            content: template_str,
            source: PathBuf::new(),
        })
    }
}

/// Errors during prompt template compilation.
#[derive(Debug, thiserror::Error)]
pub enum CompileError {
    #[error("template '{template}': parameter '{param}' declared but not found in template body")]
    MissingPlaceholder { template: String, param: String },
    #[error(
        "template '{template}': unmatched {{...}} placeholders remain after compilation: {remaining}"
    )]
    UnmatchedPlaceholders { template: String, remaining: String },
}

// ── InMemoryPromptRegistry ────────────────────────────────────────────────────

/// Registry for compiled core `PromptTemplate`s.
///
/// Templates are discovered via `PromptSource`, compiled by
/// `PromptTemplateCompiler`, then stored here by name.
pub struct InMemoryPromptRegistry {
    templates: Mutex<HashMap<String, PromptTemplate>>,
}

impl InMemoryPromptRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            templates: Mutex::new(HashMap::new()),
        }
    }

    /// Register a compiled template.
    pub fn register(&self, name: String, template: PromptTemplate) {
        self.templates.lock().unwrap().insert(name, template);
    }

    /// Look up a template by name.
    pub fn get(&self, name: &str) -> Option<PromptTemplate> {
        self.templates.lock().unwrap().get(name).cloned()
    }

    /// List all registered template names.
    pub fn list_names(&self) -> Vec<String> {
        self.templates.lock().unwrap().keys().cloned().collect()
    }

    /// Discover templates from a `PromptSource`, compile, and register them.
    ///
    /// Returns the names of all successfully registered templates.
    pub async fn ingest(&self, source: &dyn PromptSource) -> Result<Vec<String>> {
        let raw_templates = source.discover().await?;
        let mut names = Vec::new();
        for raw in &raw_templates {
            let compiled = PromptTemplateCompiler::compile(raw)
                .map_err(|e| anyhow::anyhow!("compile failed for '{}': {e}", raw.name))?;
            self.register(raw.name.clone(), compiled);
            names.push(raw.name.clone());
        }
        Ok(names)
    }

    /// Invoke a registered template with positional arguments.
    pub fn invoke(&self, name: &str, args: &[String]) -> std::result::Result<String, InvokeError> {
        let template = self
            .get(name)
            .ok_or_else(|| InvokeError::NotFound(name.to_string()))?;
        Ok(llm_harness::invoke_template(&template, args))
    }
}

impl Default for InMemoryPromptRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors returned by `InMemoryPromptRegistry::invoke`.
#[derive(Debug, thiserror::Error)]
pub enum InvokeError {
    #[error("template not found: {0}")]
    NotFound(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(test)]
    use llm_harness::invoke_template;

    #[test]
    fn compile_basic_template() {
        let source = PromptSourceTemplate {
            name: "review_code".into(),
            description: "Review code".into(),
            template: "Review {{code}} in {{lang}}".into(),
            arguments: vec![
                ArgumentDef {
                    name: "lang".into(),
                    description: "Language".into(),
                    required: true,
                },
                ArgumentDef {
                    name: "code".into(),
                    description: "Code".into(),
                    required: true,
                },
            ],
        };
        let compiled = PromptTemplateCompiler::compile(&source).unwrap();
        let rendered =
            invoke_template(&compiled, &["Rust".to_string(), "fn main() {}".to_string()]);
        assert!(rendered.contains("Review fn main() {} in Rust"));
    }

    #[test]
    fn compile_single_param() {
        let source = PromptSourceTemplate {
            name: "greet".into(),
            description: "Greet".into(),
            template: "Hello {{name}}!".into(),
            arguments: vec![ArgumentDef {
                name: "name".into(),
                description: "Name".into(),
                required: true,
            }],
        };
        let compiled = PromptTemplateCompiler::compile(&source).unwrap();
        let rendered = invoke_template(&compiled, &["World".to_string()]);
        assert_eq!(rendered, "Hello World!");
    }

    #[test]
    fn compile_missing_placeholder_errors() {
        let source = PromptSourceTemplate {
            name: "bad".into(),
            description: "Bad template".into(),
            template: "Hello!".into(),
            arguments: vec![ArgumentDef {
                name: "missing".into(),
                description: "Missing".into(),
                required: true,
            }],
        };
        let result = PromptTemplateCompiler::compile(&source);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing"));
    }

    #[test]
    fn compile_unmatched_braces_errors() {
        let source = PromptSourceTemplate {
            name: "bad".into(),
            description: "Bad template".into(),
            template: "Hello {{name}} and {{unmatched}}!".into(),
            arguments: vec![ArgumentDef {
                name: "name".into(),
                description: "Name".into(),
                required: true,
            }],
        };
        let result = PromptTemplateCompiler::compile(&source);
        assert!(result.is_err());
    }

    #[test]
    fn registry_register_and_invoke() {
        let registry = InMemoryPromptRegistry::new();
        registry.register(
            "greet".into(),
            PromptTemplate {
                name: "greet".into(),
                description: "Greet".into(),
                content: "Hello $1!".into(),
                source: PathBuf::new(),
            },
        );
        let result = registry.invoke("greet", &["World".to_string()]).unwrap();
        assert_eq!(result, "Hello World!");
    }

    #[test]
    fn registry_missing_template_errors() {
        let registry = InMemoryPromptRegistry::new();
        let result = registry.invoke("nonexistent", &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("nonexistent"));
    }

    #[tokio::test]
    async fn registry_ingest_from_source() {
        use futures::FutureExt;

        struct TestSource;
        impl PromptSource for TestSource {
            fn discover(&self) -> BoxFuture<'_, Result<Vec<PromptSourceTemplate>>> {
                async move {
                    Ok(vec![PromptSourceTemplate {
                        name: "test_tmpl".into(),
                        description: "Test".into(),
                        template: "Say {{msg}}".into(),
                        arguments: vec![ArgumentDef {
                            name: "msg".into(),
                            description: "Message".into(),
                            required: true,
                        }],
                    }])
                }
                .boxed()
            }
        }

        let registry = InMemoryPromptRegistry::new();
        let names = registry.ingest(&TestSource).await.unwrap();
        assert_eq!(names, vec!["test_tmpl"]);

        let result = registry
            .invoke("test_tmpl", &["hello".to_string()])
            .unwrap();
        assert_eq!(result, "Say hello");
    }
}
