//! Opt-in warm runtime microbenchmark, without a model or broker.
use super::fixture::{bash, python, Fixture};
use serde_json::json;
use std::time::Instant;

const WARMUP: usize = 50;
const SAMPLES: usize = 1_000;

struct Measurements {
    label: &'static str,
    fixture: Fixture,
    micros: Vec<f64>,
}

impl Measurements {
    async fn sample(&mut self, record: bool) {
        // Keep recorder growth and assertions outside the timed region.
        self.fixture.probe.calls.lock().clear();
        let args = json!({"value":"latency"});
        let start = Instant::now();
        let result = self.fixture.eval(args.clone()).await.unwrap();
        let elapsed = start.elapsed().as_secs_f64() * 1_000_000.0;
        assert_eq!(result.output, args);
        assert_eq!(self.fixture.probe.calls.lock().len(), 1);
        if record {
            self.micros.push(elapsed);
        }
    }

    fn report(&mut self, baseline: Option<f64>) -> f64 {
        self.micros.sort_by(f64::total_cmp);
        let mean = self.micros.iter().sum::<f64>() / self.micros.len() as f64;
        let median = self.micros[self.micros.len() / 2];
        let p95 = self.micros[(self.micros.len() - 1) * 95 / 100];
        println!(
            "{}: n={} mean_us={mean:.3} median_us={median:.3} p95_us={p95:.3} added_mean_us={:.3}",
            self.label,
            self.micros.len(),
            mean - baseline.unwrap_or(mean)
        );
        mean
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "opt-in latency measurement; no timing threshold"]
async fn middleware_noop_latency() {
    let mut measurements = Vec::new();
    for (label, entries) in [
        ("empty", vec![]),
        ("bash-next", vec![bash("next")]),
        ("python-call_next", vec![python("call_next()")]),
    ] {
        measurements.push(Measurements {
            label,
            fixture: Fixture::new(entries).await,
            micros: Vec::with_capacity(SAMPLES),
        });
    }
    // Interleave variants and rotate their order to reduce order/load bias.
    for index in 0..WARMUP + SAMPLES {
        for offset in 0..measurements.len() {
            let variant = (index + offset) % measurements.len();
            measurements[variant].sample(index >= WARMUP).await;
        }
    }
    println!("warmup={WARMUP} samples={SAMPLES} units=microseconds profile=test");
    let baseline = measurements[0].report(None);
    for measurement in &mut measurements[1..] {
        measurement.report(Some(baseline));
    }
}
