/// Model pricing database (USD per 1M tokens)
/// Updated April 2026
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
}

pub fn get_pricing(model: &str) -> ModelPricing {
    let m = model.to_lowercase();

    // Anthropic
    if m.contains("opus") {
        return ModelPricing { input: 15.0, output: 75.0 };
    }
    if m.contains("sonnet") {
        return ModelPricing { input: 3.0, output: 15.0 };
    }
    if m.contains("haiku") {
        return ModelPricing { input: 0.25, output: 1.25 };
    }

    // OpenAI
    if m.contains("gpt-4o-mini") {
        return ModelPricing { input: 0.15, output: 0.6 };
    }
    if m.contains("gpt-4o") {
        return ModelPricing { input: 2.5, output: 10.0 };
    }
    if m.contains("gpt-4-turbo") || m.contains("gpt-4-1") {
        return ModelPricing { input: 10.0, output: 30.0 };
    }
    if m.contains("o1") || m.contains("o3") {
        return ModelPricing { input: 15.0, output: 60.0 };
    }

    // Mistral
    if m.contains("codestral") {
        return ModelPricing { input: 0.3, output: 0.9 };
    }
    if m.contains("mistral-large") {
        return ModelPricing { input: 2.0, output: 6.0 };
    }
    if m.contains("mistral-medium") {
        return ModelPricing { input: 2.7, output: 8.1 };
    }
    if m.contains("mistral-small") || m.contains("devstral") {
        return ModelPricing { input: 0.1, output: 0.3 };
    }
    if m.contains("pixtral") {
        return ModelPricing { input: 2.0, output: 6.0 };
    }
    if m.contains("ministral") {
        return ModelPricing { input: 0.1, output: 0.1 };
    }

    // Default (conservative estimate)
    ModelPricing { input: 2.0, output: 6.0 }
}
