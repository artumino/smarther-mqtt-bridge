use std::time::Duration;

use smarther::SmartherApi;
use tokio_util::sync::CancellationToken;

use crate::{Context, refresh_token_if_needed};

const REFRESH_TOKEN_DAYS: u64 = 85;
const REFRESH_TOKEN_FAIL_INTERVAL_SECONDS: u64 = 60*5;

enum BreakType {
    Continue,
    Break,
    None
}

async fn wait_with_cancellation(context: &Context, cancellation_token: &CancellationToken, delay: Duration) -> BreakType {
    tokio::select! {
        _ = tokio::time::sleep(delay) => BreakType::None,
        _ = cancellation_token.cancelled() => BreakType::Break,
        _ = context.wait_token_reset() => BreakType::Continue
    }
}

pub(crate) async fn token_refresher(context: &Context, cancellation_token: CancellationToken) {
    let refresh_max_interval = Duration::from_secs(86400 * REFRESH_TOKEN_DAYS);
    let refresh_fail_max_interval = Duration::from_secs(REFRESH_TOKEN_FAIL_INTERVAL_SECONDS);
    
    'outer : while !cancellation_token.is_cancelled() {
        match wait_with_cancellation(context, &cancellation_token, refresh_max_interval).await {
            BreakType::Continue => { continue; }
            BreakType::Break => { break 'outer; }
            BreakType::None => {}
        }

        let client = SmartherApi::default();
        loop {
            if let Ok(auth_info) = refresh_token_if_needed(&client, context.auth_info.clone().into_inner(), &context.auth_file).await {
                context.auth_info.replace(auth_info.clone());
                break;
            }
            
            match wait_with_cancellation(context, &cancellation_token, refresh_fail_max_interval).await {
                BreakType::Continue => { continue 'outer; }
                BreakType::Break => { break 'outer; }
                BreakType::None => {}
            }
        }
    }
}
