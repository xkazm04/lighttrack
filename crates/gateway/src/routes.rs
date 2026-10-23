//! Turn the request's `model` into the chain of targets to try.
//!
//! A route name resolves to its declared primary + fallbacks. A literal `provider/model[@effort]`
//! is served as-is, followed by the `[defaults].fallback` list minus anything on the same seat —
//! so an app that has not been onboarded yet still gets the failover, just not the routing.

use crate::config::GatewayConfig;
use crate::error::GatewayError;
use crate::target::Target;

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The route name, when the request used one. This is the use case the event is filed under.
    pub route: Option<String>,
    pub chain: Vec<Target>,
}

pub fn resolve(cfg: &GatewayConfig, model: &str) -> Result<Plan, GatewayError> {
    let model = model.trim();
    if let Some(route) = cfg.routes.get(model) {
        let mut chain = vec![parse(&route.primary)?];
        for spec in &route.fallback {
            chain.push(parse(spec)?);
        }
        return Ok(Plan {
            route: Some(model.to_string()),
            chain,
        });
    }
    if !model.contains('/') {
        let known: Vec<&String> = cfg.routes.keys().collect();
        return Err(GatewayError::model_not_found(format!(
            "'{model}' is neither a route in gateway.toml nor a 'provider/model' literal; \
             routes: {known:?}"
        )));
    }
    let literal = parse(model)?;
    let mut chain = vec![literal.clone()];
    for spec in &cfg.defaults.fallback {
        let fb = parse(spec)?;
        if !fb.same_seat(&literal) && !chain.contains(&fb) {
            chain.push(fb);
        }
    }
    Ok(Plan { route: None, chain })
}

fn parse(spec: &str) -> Result<Target, GatewayError> {
    Target::parse(spec).map_err(GatewayError::bad_request)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> GatewayConfig {
        toml::from_str(
            r#"
            [defaults]
            fallback = ["codex/gpt-5.5", "anthropic/claude-haiku-4-5"]
            [routes.summarize]
            primary = "anthropic/claude-sonnet-5@medium"
            fallback = ["codex/gpt-5.5@medium"]
            "#,
        )
        .unwrap()
    }

    #[test]
    fn a_route_name_resolves_to_its_declared_chain() {
        let p = resolve(&cfg(), "summarize").unwrap();
        assert_eq!(p.route.as_deref(), Some("summarize"));
        assert_eq!(p.chain.len(), 2);
        assert_eq!(p.chain[0].spec(), "anthropic/claude-sonnet-5@medium");
    }

    #[test]
    fn a_literal_gets_the_default_fallbacks_minus_its_own_seat() {
        let p = resolve(&cfg(), "anthropic/claude-opus-5@high").unwrap();
        assert_eq!(p.route, None);
        let specs: Vec<String> = p.chain.iter().map(Target::spec).collect();
        assert_eq!(specs, ["anthropic/claude-opus-5@high", "codex/gpt-5.5"]);
    }

    #[test]
    fn an_unknown_bare_name_is_a_404_naming_the_routes() {
        let err = resolve(&cfg(), "gpt-4o").unwrap_err();
        assert_eq!(err.status, 404);
        assert!(err.message.contains("summarize"));
    }
}
