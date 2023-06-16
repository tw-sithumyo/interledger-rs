use interledger_packet::{ErrorCode, MaxPacketAmountDetails, Reject};
#[cfg(test)]
use once_cell::sync::Lazy;
use std::cmp::{max, min};
use tracing::{debug, warn};

/// A basic congestion controller that implements an
/// Additive Increase, Multiplicative Decrease (AIMD) algorithm.
///
/// Future implementations of this will use more advanced congestion
/// control algorithms.
pub struct CongestionController {
    state: CongestionState,
    /// Amount which is added to `max_in_flight` per fulfill
    increase_amount: u64,
    /// Divide `max_in_flight` by this factor per reject with code for insufficient liquidity
    /// or if there is no `max_packet_amount` specified
    decrease_factor: f64,
    /// The maximum amount we are allowed to add in a packet. This gets automatically set if
    /// we receive a reject packet with a `F08_AMOUNT_TOO_LARGE` error
    max_packet_amount: Option<u64>,
    /// The current amount in flight
    amount_in_flight: u64,
    /// The maximum allowed amount to be in flight
    max_in_flight: u64,
}

#[derive(PartialEq)]
enum CongestionState {
    SlowStart,
    AvoidCongestion,
}

impl CongestionController {
    /// Constructs a new congestion controller
    pub fn new(start_amount: u64, increase_amount: u64, decrease_factor: f64) -> Self {
        CongestionController {
            state: CongestionState::SlowStart,
            increase_amount,
            decrease_factor,
            max_packet_amount: None,
            amount_in_flight: 0,
            max_in_flight: start_amount,
        }
    }

    /// Maximium allowed packet amount allowed to send in a packet per F08s
    pub fn get_max_packet_amount(&self) -> u64 {
        self.max_packet_amount.unwrap_or(u64::max_value())
    }

    /// The maximum amount availble to be sent is the maximum amount in flight minus the current amount in flight
    pub fn get_amount_left_in_window(&self) -> u64 {
        self.max_in_flight.saturating_sub(self.amount_in_flight)
    }

    /// Increments the amount in flight by the provided amount
    pub fn prepare(&mut self, amount: u64) {
        if amount > 0 {
            self.amount_in_flight += amount;
            debug!(
                "Prepare packet of {}, amount in flight is now: {}",
                amount, self.amount_in_flight
            );
        }
    }

    /// Decrements the amount in flight by the provided amount
    /// Increases the allowed max in flight amount cap
    pub fn fulfill(&mut self, prepare_amount: u64) {
        self.amount_in_flight -= prepare_amount;

        // Before we know how much we should be sending at a time,
        // double the window size on every successful packet.
        // Once we start getting errors, switch to Additive Increase,
        // Multiplicative Decrease (AIMD) congestion avosequenceance
        if self.state == CongestionState::SlowStart {
            // Double the max in flight but don't exceed the u64 max value
            if u64::max_value() / 2 >= self.max_in_flight {
                self.max_in_flight *= 2;
            } else {
                self.max_in_flight = u64::max_value();
            }
            debug!(
                "Fulfilled packet of {}, doubling max in flight to: {}",
                prepare_amount, self.max_in_flight
            );
        } else {
            // Add to the max in flight but don't exeed the u64 max value
            if u64::max_value() - self.increase_amount >= self.max_in_flight {
                self.max_in_flight += self.increase_amount;
            } else {
                self.max_in_flight = u64::max_value();
            }
            debug!(
                "Fulfilled packet of {}, increasing max in flight to: {}",
                prepare_amount, self.max_in_flight
            );
        }
    }

    /// Decrements the amount in flight by the provided amount
    /// Decreases the allowed max in flight amount cap
    pub fn reject(&mut self, prepare_amount: u64, reject: &Reject) {
        self.amount_in_flight -= prepare_amount;

        match reject.code() {
            ErrorCode::T04_INSUFFICIENT_LIQUIDITY => {
                self.state = CongestionState::AvoidCongestion;
                self.max_in_flight = max(
                    (self.max_in_flight as f64 / self.decrease_factor).floor() as u64,
                    1,
                );
                debug!("Rejected packet with T04 error. Amount in flight was: {}, decreasing max in flight to: {}", self.amount_in_flight + prepare_amount, self.max_in_flight);
            }
            ErrorCode::F08_AMOUNT_TOO_LARGE => {
                if let Ok(details) = MaxPacketAmountDetails::from_bytes(reject.data()) {
                    let new_max_packet_amount: u64 =
                        prepare_amount * details.max_amount() / details.amount_received();
                    if let Some(max_packet_amount) = self.max_packet_amount {
                        self.max_packet_amount =
                            Some(min(max_packet_amount, new_max_packet_amount));
                    } else {
                        self.max_packet_amount = Some(new_max_packet_amount);
                    }
                } else {
                    warn!("Got F08: Amount Too Large Error without max packet amount details attached");
                    if let Some(max_packet_amount) = self.max_packet_amount {
                        self.max_packet_amount =
                            Some((max_packet_amount as f64 / self.decrease_factor) as u64);
                    }
                }
            }
            _ => {
                // No special treatment for other errors
            }
        }
    }

    #[cfg(test)]
    fn set_max_packet_amount(&mut self, max_packet_amount: u64) {
        self.max_packet_amount = Some(max_packet_amount)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod slow_start {
        use super::*;

        #[test]
        fn doubles_max_amount_on_fulfill() {
            let mut controller = CongestionController::new(1000, 1000, 2.0);

            let amount = controller.get_amount_left_in_window();
            controller.prepare(amount);
            controller.fulfill(amount);
            assert_eq!(controller.get_amount_left_in_window(), 2000);

            let amount = controller.get_amount_left_in_window();
            controller.prepare(amount);
            controller.fulfill(amount);
            assert_eq!(controller.get_amount_left_in_window(), 4000);

            let amount = controller.get_amount_left_in_window();
            controller.prepare(amount);
            controller.fulfill(amount);
            assert_eq!(controller.get_amount_left_in_window(), 8000);
        }

        #[test]
        fn doesnt_overflow_u64() {
            let mut controller = CongestionController {
                state: CongestionState::SlowStart,
                increase_amount: 1000,
                decrease_factor: 2.0,
                max_packet_amount: None,
                amount_in_flight: 0,
                max_in_flight: u64::max_value() - 1,
            };

            let amount = controller.get_amount_left_in_window();
            controller.prepare(amount);
            controller.fulfill(amount);
            assert_eq!(controller.get_amount_left_in_window(), u64::max_value());
        }
    }

    mod congestion_avoidance {
        use super::*;
        use interledger_packet::RejectBuilder;

        static INSUFFICIENT_LIQUIDITY_ERROR: Lazy<Reject> = Lazy::new(|| {
            RejectBuilder {
                code: ErrorCode::T04_INSUFFICIENT_LIQUIDITY,
                message: &[],
                triggered_by: None,
                data: &[],
            }
            .build()
        });

        #[test]
        fn additive_increase() {
            let mut controller = CongestionController::new(1000, 1000, 2.0);
            controller.state = CongestionState::AvoidCongestion;
            for i in 1..5 {
                let amount = i * 1000;
                controller.prepare(amount);
                controller.fulfill(amount);
                assert_eq!(controller.get_amount_left_in_window(), 1000 + i * 1000);
            }
        }

        #[test]
        fn multiplicative_decrease() {
            let mut controller = CongestionController::new(1000, 1000, 2.0);
            controller.state = CongestionState::AvoidCongestion;

            let amount = controller.get_amount_left_in_window();
            controller.prepare(amount);
            controller.reject(amount, &INSUFFICIENT_LIQUIDITY_ERROR);
            assert_eq!(controller.get_amount_left_in_window(), 500);

            let amount = controller.get_amount_left_in_window();
            controller.prepare(amount);
            controller.reject(amount, &INSUFFICIENT_LIQUIDITY_ERROR);
            assert_eq!(controller.get_amount_left_in_window(), 250);
        }

        #[test]
        fn aimd_combined() {
            let mut controller = CongestionController::new(1000, 1000, 2.0);
            controller.state = CongestionState::AvoidCongestion;

            let amount = controller.get_amount_left_in_window();
            controller.prepare(amount);
            controller.fulfill(amount);
            assert_eq!(controller.get_amount_left_in_window(), 2000);

            let amount = controller.get_amount_left_in_window();
            controller.prepare(amount);
            controller.fulfill(amount);
            assert_eq!(controller.get_amount_left_in_window(), 3000);

            let amount = controller.get_amount_left_in_window();
            controller.prepare(amount);
            controller.reject(amount, &INSUFFICIENT_LIQUIDITY_ERROR);
            assert_eq!(controller.get_amount_left_in_window(), 1500);

            let amount = controller.get_amount_left_in_window();
            controller.prepare(amount);
            controller.fulfill(amount);
            assert_eq!(controller.get_amount_left_in_window(), 2500);
        }

        #[test]
        fn max_packet_amount() {
            let mut controller = CongestionController::new(1000, 1000, 2.0);
            assert_eq!(controller.get_amount_left_in_window(), 1000);

            controller.prepare(1000);
            controller.reject(
                1000,
                &RejectBuilder {
                    code: ErrorCode::F08_AMOUNT_TOO_LARGE,
                    message: &[],
                    triggered_by: None,
                    data: &MaxPacketAmountDetails::new(100, 10).to_bytes(),
                }
                .build(),
            );
            assert_eq!(controller.get_max_packet_amount(), 100);

            let mut amount = min(
                controller.get_max_packet_amount(),
                controller.get_amount_left_in_window(),
            );
            controller.prepare(amount);
            controller.reject(
                amount,
                &RejectBuilder {
                    code: ErrorCode::F08_AMOUNT_TOO_LARGE,
                    message: &[],
                    triggered_by: None,
                    data: &[],
                }
                .build(),
            );
            // it was decreased by the decrease factor
            assert_eq!(controller.get_max_packet_amount(), amount / 2);

            amount = min(
                controller.get_max_packet_amount(),
                controller.get_amount_left_in_window(),
            );
            controller.prepare(amount);
            controller.fulfill(amount);

            amount = min(
                controller.get_max_packet_amount(),
                controller.get_amount_left_in_window(),
            );
            assert_eq!(amount, 50);
        }

        #[test]
        fn max_packet_amount_doesnt_overflow_u64() {
            let mut controller = CongestionController::new(1000, 1000, 5.0);

            controller.prepare(500);
            controller.prepare(500);
            controller.reject(500, &INSUFFICIENT_LIQUIDITY_ERROR);

            assert_eq!(controller.get_amount_left_in_window(), 0);
        }

        #[test]
        fn doesnt_overflow_u64() {
            let mut controller = CongestionController {
                state: CongestionState::AvoidCongestion,
                increase_amount: 1000,
                decrease_factor: 2.0,
                max_packet_amount: None,
                amount_in_flight: 0,
                max_in_flight: u64::max_value() - 1,
            };

            let amount = controller.get_amount_left_in_window();
            controller.prepare(amount);
            controller.fulfill(amount);
            assert_eq!(controller.get_amount_left_in_window(), u64::max_value());
        }
    }

    mod tracking_amount_in_flight {
        use super::*;

        #[test]
        fn tracking_amount_in_flight() {
            let mut controller = CongestionController::new(1000, 1000, 2.0);
            controller.set_max_packet_amount(600);
            assert_eq!(controller.get_max_packet_amount(), 600);

            controller.prepare(100);
            let mut max_amount = min(
                controller.get_amount_left_in_window(),
                controller.get_max_packet_amount(),
            );
            assert_eq!(max_amount, 600);

            controller.prepare(600);
            max_amount = min(
                controller.get_amount_left_in_window(),
                controller.get_max_packet_amount(),
            );
            assert_eq!(max_amount, 1000 - 600 - 100);
        }
    }
}
