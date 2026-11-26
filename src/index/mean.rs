//! Pool independently normalized chunk vectors into one unit-length repository vector.
use anyhow::{Result, ensure};

#[derive(Default)]
pub(crate) struct Mean {
    sum: Vec<f64>,
}

impl Mean {
    pub(crate) fn push(&mut self, vector: &[f32]) -> Result<()> {
        let norm = vector
            .iter()
            .map(|&x| f64::from(x).powi(2))
            .sum::<f64>()
            .sqrt();
        ensure!(
            norm.is_finite() && norm > 0.0,
            "模型返回了空向量、全零向量或非有限值"
        );
        if self.sum.is_empty() {
            self.sum.resize(vector.len(), 0.0);
        }
        ensure!(
            self.sum.len() == vector.len(),
            "模型返回的向量维度不一致，请检查模型并运行 starstack index --rebuild"
        );
        for (sum, &x) in self.sum.iter_mut().zip(vector) {
            *sum += f64::from(x) / norm;
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<Vec<f32>> {
        let norm = self.sum.iter().map(|x| x * x).sum::<f64>().sqrt();
        ensure!(
            norm.is_finite() && norm > 0.0,
            "文本分块的平均向量为零或无效"
        );
        Ok(self.sum.into_iter().map(|x| (x / norm) as f32).collect())
    }
}
