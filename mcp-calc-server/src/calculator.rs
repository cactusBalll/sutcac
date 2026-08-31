//! Simple calculator MCP service.
//!
//! Exposes five arithmetic tools:
//! - `sum(a, b)` -> a + b
//! - `sub(a, b)`  -> a - b
//! - `mul(a, b)`  -> a * b
//! - `div(a, b)`  -> a / b (errors on division by zero)
//! - `modulo(a, b)` -> a % b (errors on division by zero)

use rmcp::{
    ErrorData, handler::server::wrapper::Parameters, model::ErrorCode, schemars, tool, tool_router,
};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CalcRequest {
    /// Left-hand operand.
    pub a: f64,
    /// Right-hand operand.
    pub b: f64,
}

#[derive(Debug, Clone)]
pub struct Calculator;

impl Default for Calculator {
    fn default() -> Self {
        Self
    }
}

#[tool_router(server_handler)]
impl Calculator {
    #[tool(description = "Calculate the sum of two numbers")]
    fn sum(&self, Parameters(CalcRequest { a, b }): Parameters<CalcRequest>) -> String {
        format_number(a + b)
    }

    #[tool(description = "Calculate the difference of two numbers")]
    fn sub(&self, Parameters(CalcRequest { a, b }): Parameters<CalcRequest>) -> String {
        format_number(a - b)
    }

    #[tool(description = "Calculate the product of two numbers")]
    fn mul(&self, Parameters(CalcRequest { a, b }): Parameters<CalcRequest>) -> String {
        format_number(a * b)
    }

    #[tool(description = "Divide the first number by the second")]
    fn div(
        &self,
        Parameters(CalcRequest { a, b }): Parameters<CalcRequest>,
    ) -> Result<String, ErrorData> {
        if b == 0.0 {
            return Err(ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                "division by zero",
                None,
            ));
        }
        Ok(format_number(a / b))
    }

    #[tool(description = "Calculate the modulo (remainder) of two numbers")]
    fn modulo(
        &self,
        Parameters(CalcRequest { a, b }): Parameters<CalcRequest>,
    ) -> Result<String, ErrorData> {
        if b == 0.0 {
            return Err(ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                "division by zero",
                None,
            ));
        }
        Ok(format_number(a % b))
    }
}

/// Format a floating-point result, trimming redundant `.0` for integer values.
fn format_number(n: f64) -> String {
    if n.is_nan() {
        return "NaN".to_string();
    }
    if n.is_infinite() {
        return if n.is_sign_positive() {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
    }
    if n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0 {
        format!("{:.0}", n)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_number_trims_integer_decimals() {
        assert_eq!(format_number(5.0), "5");
        assert_eq!(format_number(-3.0), "-3");
    }

    #[test]
    fn format_number_keeps_fractions() {
        assert_eq!(format_number(1.5), "1.5");
        assert_eq!(format_number(0.1 + 0.2), "0.30000000000000004");
    }

    #[test]
    fn sum_and_sub_tools() {
        let calc = Calculator;
        assert_eq!(calc.sum(Parameters(CalcRequest { a: 3.0, b: 2.0 })), "5");
        assert_eq!(calc.sub(Parameters(CalcRequest { a: 3.0, b: 2.0 })), "1");
    }

    #[test]
    fn div_errors_on_zero() {
        let calc = Calculator;
        let result = calc.div(Parameters(CalcRequest { a: 1.0, b: 0.0 }));
        assert!(result.is_err());
    }
}
