//! Experimental pool settings for the standalone Kokoro benchmark.

pub struct RuntimeSettings {
    pub threads: usize,
    pub spin: bool,
    pub flush_denormals: bool,
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            threads: 0, // ORT chooses the host's core count; don't underfill it.
            spin: true, // Preserve ORT's normal, bounded work-wait spinning.
            flush_denormals: false,
        }
    }
}

impl RuntimeSettings {
    pub fn pool(&self) -> ort::Result<ort::environment::GlobalThreadPoolOptions> {
        let pool = ort::environment::GlobalThreadPoolOptions::default()
            .with_intra_threads(self.threads)?
            .with_inter_threads(1)?
            .with_spin_control(self.spin)?;
        if self.flush_denormals {
            pool.with_flush_to_zero()
        } else {
            Ok(pool)
        }
    }
}
