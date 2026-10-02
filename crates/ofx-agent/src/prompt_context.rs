use crate::compactor::text_tokens;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RequestCost {
    pub(crate) bytes: usize,
    pub(crate) text_tokens: usize,
    pub(crate) estimated_tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Calibration {
    pub(crate) model: String,
    pub(crate) request: RequestCost,
    pub(crate) exact_input_tokens: usize,
}

impl RequestCost {
    pub(crate) fn measure(body: &str) -> Self {
        let tokens = text_tokens(body);
        Self {
            bytes: body.len(),
            text_tokens: tokens,
            estimated_tokens: tokens,
        }
    }

    pub(crate) fn calibrated(self, calibration: &Calibration) -> Self {
        if calibration.request.bytes == 0 || calibration.exact_input_tokens == 0 {
            return self;
        }
        let tokens = multiply_divide_ceil(
            self.bytes,
            calibration.exact_input_tokens,
            calibration.request.bytes,
        );
        Self {
            estimated_tokens: tokens.max(1),
            ..self
        }
    }
}

fn multiply_divide_ceil(value: usize, numerator: usize, denominator: usize) -> usize {
    let product =
        u128::try_from(value).unwrap_or(u128::MAX) * u128::try_from(numerator).unwrap_or(u128::MAX);
    let denominator = u128::try_from(denominator).unwrap_or(u128::MAX);
    usize::try_from(product.div_ceil(denominator)).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests;
