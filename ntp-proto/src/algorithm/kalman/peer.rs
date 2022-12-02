use tracing::{debug, info, trace};

use crate::{
    Measurement, NtpDuration, NtpPacket, NtpTimestamp, PollInterval, PollIntervalLimits,
    SystemConfig,
};

use super::{
    config::AlgorithmConfig,
    matrix::{Matrix, Vector},
    sqr, PeerSnapshot,
};

#[derive(Debug, Default, Copy, Clone)]
struct RttBuf {
    data: [f64; 8],
    next_idx: usize,
}

impl RttBuf {
    fn mean(&self) -> f64 {
        self.data.iter().sum::<f64>() / 8.
    }

    fn var(&self) -> f64 {
        let mean = self.mean();
        self.data.iter().map(|v| sqr(v - mean)).sum::<f64>() / 7.
    }

    fn update(&mut self, rtt: f64) {
        self.data[self.next_idx] = rtt;
        self.next_idx = (self.next_idx + 1) % 8;
    }
}

#[derive(Debug, Clone)]
struct InitialPeerFilter {
    rtt_stats: RttBuf,
    init_offset: RttBuf,

    samples: i32,
}

impl InitialPeerFilter {
    fn update(&mut self, measurement: Measurement) {
        self.rtt_stats.update(measurement.delay.to_seconds());
        self.init_offset.update(measurement.offset.to_seconds());
        self.samples += 1;
        debug!(samples = self.samples, "Initial peer update");
    }

    fn process_offset_steering(&mut self, steer: f64) {
        for ref mut sample in self.init_offset.data {
            *sample -= steer;
        }
    }
}

#[derive(Debug, Clone)]
struct PeerFilter {
    state: Vector,
    uncertainty: Matrix,
    clock_wander: f64,

    rtt_stats: RttBuf,

    precision_score: i32,
    poll_score: i32,
    desired_poll_interval: PollInterval,

    last_measurement: Measurement,
    last_packet: NtpPacket<'static>,
    prev_was_outlier: bool,

    // Last time a packet was processed
    last_iter: NtpTimestamp,
    // Current time of the filter state.
    filter_time: NtpTimestamp,
}

impl PeerFilter {
    /// Move the filter forward to reflect the situation at a new, later timestamp
    fn progress_filtertime(&mut self, time: NtpTimestamp) {
        debug_assert!(time >= self.filter_time);
        if time < self.filter_time {
            return;
        }

        // Time step paremeters
        let delta_t = (time - self.filter_time).to_seconds();
        let update = Matrix::new(1.0, delta_t, 0.0, 1.0);
        let process_noise = Matrix::new(
            self.clock_wander * delta_t * delta_t * delta_t / 3.,
            self.clock_wander * delta_t * delta_t / 2.,
            self.clock_wander * delta_t * delta_t / 2.,
            self.clock_wander * delta_t,
        );

        // Kalman filter update
        self.state = update * self.state;
        self.uncertainty = update * self.uncertainty * update.transpose() + process_noise;
        self.filter_time = time;

        trace!(?time, "Filter progressed");
    }

    /// Absorb knowledge from a measurement
    fn absorb_measurement(&mut self, measurement: Measurement) -> (f64, f64, f64) {
        // Measurement paramaters
        let delay_variance = self.rtt_stats.var();
        let m_delta_t = (measurement.localtime - self.last_measurement.localtime).to_seconds();

        // Kalman filter update
        let measurement_vec = Vector::new(
            measurement.offset.to_seconds(),
            (measurement.offset - self.last_measurement.offset).to_seconds(),
        );
        let measurement_transform = Matrix::new(1., 0., 0., m_delta_t);
        let measurement_noise = Matrix::new(
            delay_variance / 4.,
            delay_variance / 4.,
            delay_variance / 4.,
            delay_variance / 2.,
        );
        let difference = measurement_vec - measurement_transform * self.state;
        let difference_covariance =
            measurement_transform * self.uncertainty * measurement_transform.transpose()
                + measurement_noise;
        let update_strength =
            self.uncertainty * measurement_transform.transpose() * difference_covariance.inverse();
        self.state = self.state + update_strength * difference;
        self.uncertainty = ((Matrix::UNIT - update_strength * measurement_transform)
            * self.uncertainty)
            .symmetrize();

        // Statistics
        let chi = difference.inner(difference_covariance.inverse() * difference);
        let weight = measurement_noise.determinant() / difference_covariance.determinant();

        self.last_measurement = measurement;

        trace!(chi, weight = 1. - weight, "Measurement absorbed");

        (chi, 1. - weight, m_delta_t)
    }

    /// Ensure we poll often enough to keep the filter well-fed with information, but
    /// not so much that each individual poll message gives us very little new information.
    fn update_desired_poll(
        &mut self,
        config: &SystemConfig,
        algo_config: &AlgorithmConfig,
        chi: f64,
        weight: f64,
        measurement_period: f64,
    ) {
        // We dont want to speed up when we already want more than we get, and vice versa.
        let reference_measurement_period = self.desired_poll_interval.as_duration().to_seconds();
        if weight < algo_config.poll_low_weight
            && measurement_period / reference_measurement_period > 0.75
        {
            self.poll_score -= 1;
        } else if weight > algo_config.poll_high_weight
            && measurement_period / reference_measurement_period < 1.4
        {
            self.poll_score += 1;
        } else {
            self.poll_score -= self.poll_score.signum();
        }
        trace!(poll_score = self.poll_score, ?weight, "Poll desire update");
        if chi >= -2. * algo_config.poll_jump_threshold.ln() {
            self.desired_poll_interval = config.poll_limits.min;
            self.poll_score = 0;
        } else if self.poll_score <= -algo_config.poll_histeresis {
            self.desired_poll_interval = self.desired_poll_interval.inc(config.poll_limits);
            self.poll_score = 0;
            info!(interval = ?self.desired_poll_interval, "Increased poll interval");
        } else if self.poll_score >= algo_config.poll_histeresis {
            self.desired_poll_interval = self.desired_poll_interval.dec(config.poll_limits);
            self.poll_score = 0;
            info!(interval = ?self.desired_poll_interval, "Decreased poll interval");
        }
    }

    // Our estimate for the clock stability might be completely wrong. The code here
    // correlates the estimation for errors to what we actually observe, so we can
    // update our estimate should it turn out to be significantly off.
    fn update_wander_estimate(&mut self, algo_config: &AlgorithmConfig, chi: f64, weight: f64) {
        // Note that chi is exponentially distributed with mean 2
        // Also, we do not steer towards a smaller precision estimate when measurement noise dominates.
        if chi < -2. * (1. - algo_config.precision_low_probability).ln()
            && weight > algo_config.precision_min_weight
        {
            self.precision_score -= 1;
        } else if chi > -2. * (1. - algo_config.precision_high_probability).ln() {
            self.precision_score += 1;
        } else {
            self.precision_score -= self.precision_score.signum()
        }
        trace!(
            precision_score = self.precision_score,
            chi,
            "Wander estimate update"
        );
        if self.precision_score <= -algo_config.precision_histeresis {
            self.clock_wander /= 4.0;
            self.precision_score = 0;
            debug!(
                wander = self.clock_wander.sqrt(),
                "Decreased wander estimate"
            );
        } else if self.precision_score >= algo_config.precision_histeresis {
            self.clock_wander *= 4.0;
            self.precision_score = 0;
            debug!(
                wander = self.clock_wander.sqrt(),
                "Increased wander estimate"
            );
        }
    }

    /// Update our estimates based on a new measurement.
    fn update(
        &mut self,
        config: &SystemConfig,
        algo_config: &AlgorithmConfig,
        measurement: Measurement,
        packet: NtpPacket<'static>,
    ) -> bool {
        // Always keep the packet as last packet, since it reflects the most up-to-date
        // information on the synchronization quality of the remote and any upcoming leap seconds.
        self.last_packet = packet;

        if measurement.localtime < self.filter_time {
            // Ignore the past
            return false;
        }

        // Filter out one-time outliers (based on delay!)
        if !self.prev_was_outlier
            && (measurement.delay.to_seconds() - self.rtt_stats.mean())
                > algo_config.delay_outlier_threshold * self.rtt_stats.var().sqrt()
        {
            self.prev_was_outlier = true;
            self.last_iter = measurement.localtime;
            return false;
        }

        // Environment update
        self.progress_filtertime(measurement.localtime);
        self.rtt_stats.update(measurement.delay.to_seconds());

        let (chi, weight, measurement_period) = self.absorb_measurement(measurement);

        self.update_wander_estimate(algo_config, chi, weight);
        self.update_desired_poll(config, algo_config, chi, weight, measurement_period);

        debug!(
            "peer offset {}+-{}ms, freq {}+-{}ppm",
            self.state.entry(0) * 1000.,
            (self.uncertainty.entry(0, 0) + sqr(self.last_packet.root_dispersion().to_seconds()))
                .sqrt()
                * 1000.,
            self.state.entry(1) * 1e6,
            self.uncertainty.entry(1, 1).sqrt() * 1e6
        );

        true
    }

    fn process_offset_steering(&mut self, steer: f64) {
        self.state = self.state - Vector::new(steer, 0.0);
        self.last_measurement.offset -= NtpDuration::from_seconds(steer);
        self.last_measurement.localtime += NtpDuration::from_seconds(steer);
    }

    fn process_frequency_steering(&mut self, time: NtpTimestamp, steer: f64) {
        self.progress_filtertime(time);
        self.state = self.state - Vector::new(0.0, steer);
        self.last_measurement.offset -= NtpDuration::from_seconds(
            steer * (time - self.last_measurement.localtime).to_seconds(),
        );
    }
}

#[derive(Debug, Clone)]
enum PeerStateInner {
    Initial(InitialPeerFilter),
    Stable(PeerFilter),
}

#[derive(Debug, Clone)]
pub(super) struct PeerState(PeerStateInner);

impl PeerState {
    pub fn new() -> Self {
        PeerState(PeerStateInner::Initial(InitialPeerFilter {
            rtt_stats: RttBuf::default(),
            init_offset: RttBuf::default(),
            samples: 0,
        }))
    }

    pub fn update(
        &mut self,
        config: &SystemConfig,
        algo_config: &AlgorithmConfig,
        measurement: Measurement,
        packet: NtpPacket<'static>,
    ) -> bool {
        match &mut self.0 {
            PeerStateInner::Initial(filter) => {
                filter.update(measurement);
                if filter.samples == 8 {
                    *self = PeerState(PeerStateInner::Stable(PeerFilter {
                        state: Vector::new(filter.init_offset.mean(), 0.),
                        uncertainty: Matrix::new(
                            filter.init_offset.var(),
                            0.,
                            0.,
                            sqr(algo_config.initial_frequency_uncertainty),
                        ),
                        clock_wander: sqr(algo_config.initial_wander),
                        rtt_stats: filter.rtt_stats,
                        precision_score: 0,
                        poll_score: 0,
                        desired_poll_interval: config.initial_poll,
                        last_measurement: measurement,
                        last_packet: packet,
                        prev_was_outlier: false,
                        last_iter: measurement.localtime,
                        filter_time: measurement.localtime,
                    }));
                    debug!("Initial peer measurements complete");
                    true
                } else {
                    false
                }
            }
            PeerStateInner::Stable(filter) => {
                filter.update(config, algo_config, measurement, packet)
            }
        }
    }

    pub fn snapshot<Index: Copy>(&self, index: Index) -> Option<PeerSnapshot<Index>> {
        match &self.0 {
            PeerStateInner::Initial(_) => None,
            PeerStateInner::Stable(filter) => Some(PeerSnapshot {
                index,
                state: filter.state,
                uncertainty: filter.uncertainty,
                delay: filter.rtt_stats.mean(),
                peer_uncertainty: filter.last_packet.root_dispersion(),
                peer_delay: filter.last_packet.root_delay(),
                leap_indicator: filter.last_packet.leap(),
                last_update: filter.last_iter,
            }),
        }
    }

    pub fn get_filtertime(&self) -> Option<NtpTimestamp> {
        match &self.0 {
            PeerStateInner::Initial(_) => None,
            PeerStateInner::Stable(filter) => Some(filter.filter_time),
        }
    }

    pub fn get_desired_poll(&self, limits: &PollIntervalLimits) -> PollInterval {
        match &self.0 {
            PeerStateInner::Initial(_) => limits.min,
            PeerStateInner::Stable(filter) => filter.desired_poll_interval,
        }
    }

    pub fn progress_filtertime(&mut self, time: NtpTimestamp) {
        match &mut self.0 {
            PeerStateInner::Initial(_) => {}
            PeerStateInner::Stable(filter) => filter.progress_filtertime(time),
        }
    }

    pub fn process_offset_steering(&mut self, steer: f64) {
        match &mut self.0 {
            PeerStateInner::Initial(filter) => filter.process_offset_steering(steer),
            PeerStateInner::Stable(filter) => filter.process_offset_steering(steer),
        }
    }

    pub fn process_frequency_steering(&mut self, time: NtpTimestamp, steer: f64) {
        match &mut self.0 {
            PeerStateInner::Initial(_) => {}
            PeerStateInner::Stable(filter) => filter.process_frequency_steering(time, steer),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{Measurement, NtpInstant, NtpPacket, PollIntervalLimits};

    use super::*;

    #[test]
    fn test_offset_steering() {
        let base = NtpTimestamp::from_fixed_int(0);
        let basei = NtpInstant::now();
        let mut peer = PeerFilter {
            state: Vector::new(20e-3, 0.),
            uncertainty: Matrix::new(1e-6, 0., 0., 1e-8),
            clock_wander: 1e-8,
            rtt_stats: RttBuf {
                data: [0.0, 0.0, 0.0, 0.0, 0.875e-6, 0.875e-6, 0.875e-6, 0.875e-6],
                next_idx: 0,
            },
            precision_score: 0,
            poll_score: 0,
            desired_poll_interval: PollIntervalLimits::default().min,
            last_measurement: Measurement {
                delay: NtpDuration::from_seconds(0.0),
                offset: NtpDuration::from_seconds(20e-3),
                localtime: base,
                monotime: basei,
            },
            last_packet: NtpPacket::poll_message(PollIntervalLimits::default().min).0,
            prev_was_outlier: false,
            last_iter: base,
            filter_time: base,
        };

        peer.process_offset_steering(20e-3);

        peer.update(
            &SystemConfig::default(),
            &AlgorithmConfig::default(),
            Measurement {
                delay: NtpDuration::from_seconds(0.0),
                offset: NtpDuration::from_seconds(20e-3),
                localtime: base + NtpDuration::from_seconds(1000.0),
                monotime: basei,
            },
            NtpPacket::poll_message(PollIntervalLimits::default().min).0,
        );

        assert!((peer.state.entry(0) - 20e-3) < 1e-7);
        assert!((peer.state.entry(1) - 20e-6) < 1e-7);
    }
}