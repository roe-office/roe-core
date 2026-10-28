//! 统计计算(纯函数,无 IO,不依赖图表)。
//! 供直方图、箱线图以及后续数据分析功能复用。

/// 分位数算法:Exclusive 对应 Excel QUARTILE.EXC,Inclusive 对应 QUARTILE.INC。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuartileMethod {
    Exclusive,
    Inclusive,
}

/// 过滤掉 NaN/无穷并升序排序。
pub fn sorted_finite(values: &[f64]) -> Vec<f64> {
    let mut v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v
}

pub fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    Some(values.iter().sum::<f64>() / values.len() as f64)
}

/// 样本标准差(n-1)。少于 2 个数据返回 None。
pub fn sample_std(values: &[f64]) -> Option<f64> {
    let n = values.len();
    if n < 2 {
        return None;
    }
    let m = mean(values)?;
    let ss: f64 = values.iter().map(|x| (x - m) * (x - m)).sum();
    Some((ss / (n as f64 - 1.0)).sqrt())
}

/// 分位数,`sorted` 必须已升序。p 取 0~1。
pub fn quantile(sorted: &[f64], p: f64, method: QuartileMethod) -> Option<f64> {
    let n = sorted.len();
    if n == 0 {
        return None;
    }
    if n == 1 {
        return Some(sorted[0]);
    }
    let pos = match method {
        QuartileMethod::Exclusive => p * (n as f64 + 1.0),
        QuartileMethod::Inclusive => p * (n as f64 - 1.0) + 1.0,
    };
    let pos = pos.clamp(1.0, n as f64);
    let lo = pos.floor() as usize;
    if lo >= n {
        return Some(sorted[n - 1]);
    }
    let frac = pos - lo as f64;
    Some(sorted[lo - 1] + frac * (sorted[lo] - sorted[lo - 1]))
}

/// 箱线图统计结果。
#[derive(Debug, Clone, PartialEq)]
pub struct BoxStats {
    pub n: usize,
    pub min: f64,
    pub max: f64,
    pub q1: f64,
    pub median: f64,
    pub q3: f64,
    pub mean: f64,
    /// 下须线端点:落在 [Q1-1.5IQR, Q3+1.5IQR] 内的最小数据点
    pub lower_whisker: f64,
    /// 上须线端点:落在围栏内的最大数据点
    pub upper_whisker: f64,
    /// 围栏之外的离群点(升序)
    pub outliers: Vec<f64>,
}

pub fn box_stats(values: &[f64], method: QuartileMethod) -> Option<BoxStats> {
    let s = sorted_finite(values);
    if s.is_empty() {
        return None;
    }
    let q1 = quantile(&s, 0.25, method)?;
    let median = quantile(&s, 0.5, method)?;
    let q3 = quantile(&s, 0.75, method)?;
    let iqr = q3 - q1;
    let lo_fence = q1 - 1.5 * iqr;
    let hi_fence = q3 + 1.5 * iqr;
    let lower_whisker = s.iter().copied().find(|x| *x >= lo_fence).unwrap_or(s[0]);
    let upper_whisker = s.iter().copied().rev().find(|x| *x <= hi_fence).unwrap_or(s[s.len() - 1]);
    let outliers: Vec<f64> = s.iter().copied().filter(|x| *x < lo_fence || *x > hi_fence).collect();
    Some(BoxStats {
        n: s.len(),
        min: s[0],
        max: s[s.len() - 1],
        q1,
        median,
        q3,
        mean: mean(&s)?,
        lower_whisker,
        upper_whisker,
        outliers,
    })
}

/// 直方图结果:edges 长度 = counts 长度 + 1。
#[derive(Debug, Clone, PartialEq)]
pub struct Histogram {
    pub edges: Vec<f64>,
    pub counts: Vec<usize>,
}

/// 自动组距:Scott 正态参考规则 3.5·s/n^(1/3);数据全为整数时组距四舍五入取整(至少 1)。
/// 注意:整数取整这一步是按单个 WPS 样本推测的,待更多样本校准。
pub fn auto_bin_width(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    let range = sorted.last().copied().unwrap_or(0.0) - sorted.first().copied().unwrap_or(0.0);
    let sd = sample_std(sorted).unwrap_or(0.0);
    let mut w = 3.5 * sd / (n as f64).cbrt();
    if !(w > 0.0) || !w.is_finite() {
        return if range > 0.0 { range } else { 1.0 };
    }
    if sorted.iter().all(|x| x.fract() == 0.0) {
        w = w.round().max(1.0);
    }
    w
}

/// 分箱。`width` 与 `count` 都为 None 时自动;两者都给时以 `width` 为准。
/// `closed_right`=true:区间 (lo, hi],第一组为 [lo, hi];否则 [lo, hi),最后一组含右端点。
pub fn histogram(
    values: &[f64],
    width: Option<f64>,
    count: Option<usize>,
    closed_right: bool,
) -> Option<Histogram> {
    let s = sorted_finite(values);
    if s.is_empty() {
        return None;
    }
    let min = s[0];
    let max = s[s.len() - 1];
    let range = max - min;
    if range == 0.0 {
        return Some(Histogram { edges: vec![min, min + 1.0], counts: vec![s.len()] });
    }
    const EPS: f64 = 1e-9;
    let (w, k) = match (width, count) {
        (Some(w), _) if w > 0.0 => (w, ((range / w) - EPS).ceil().max(1.0) as usize),
        (_, Some(c)) if c > 0 => (range / c as f64, c),
        _ => {
            let w = auto_bin_width(&s);
            (w, ((range / w) - EPS).ceil().max(1.0) as usize)
        }
    };
    let k = k.min(10_000);
    let edges: Vec<f64> = (0..=k).map(|i| min + i as f64 * w).collect();
    let mut counts = vec![0usize; k];
    for v in &s {
        let t = (v - min) / w;
        let idx = if closed_right {
            (t - EPS).ceil() as i64 - 1
        } else {
            (t + EPS).floor() as i64
        };
        counts[idx.clamp(0, k as i64 - 1) as usize] += 1;
    }
    Some(Histogram { edges, counts })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WPS 样本(图表渲染测试hist_box.docx 直方图):76 个整数
    fn wps_hist_data() -> Vec<f64> {
        let pairs = [
            (1, 1), (3, 3), (5, 1), (6, 3), (7, 1), (8, 2), (9, 5), (10, 6), (11, 6),
            (12, 6), (13, 5), (14, 6), (15, 8), (16, 4), (17, 6), (18, 4), (19, 3),
            (20, 1), (21, 1), (22, 2), (24, 2),
        ];
        let mut v = Vec::new();
        for (x, c) in pairs {
            for _ in 0..c {
                v.push(x as f64);
            }
        }
        v
    }

    #[test]
    fn wps_sample_has_76_points() {
        assert_eq!(wps_hist_data().len(), 76);
    }

    #[test]
    fn histogram_auto_matches_wps_sample() {
        let h = histogram(&wps_hist_data(), None, None, true).unwrap();
        assert_eq!(h.edges, vec![1.0, 5.0, 9.0, 13.0, 17.0, 21.0, 25.0]);
        assert_eq!(h.counts, vec![5, 11, 23, 24, 9, 4]);
    }

    #[test]
    fn histogram_left_closed_last_includes_max() {
        let h = histogram(&[0.0, 1.0, 2.0, 3.0, 4.0], Some(2.0), None, false).unwrap();
        assert_eq!(h.edges, vec![0.0, 2.0, 4.0]);
        assert_eq!(h.counts, vec![2, 3]);
    }

    #[test]
    fn histogram_by_count() {
        let h = histogram(&[0.0, 10.0, 5.0, 5.0], None, Some(2), true).unwrap();
        assert_eq!(h.edges, vec![0.0, 5.0, 10.0]);
        assert_eq!(h.counts, vec![3, 1]);
    }

    #[test]
    fn histogram_constant_data() {
        let h = histogram(&[3.0, 3.0, 3.0], None, None, true).unwrap();
        assert_eq!(h.counts, vec![3]);
    }

    #[test]
    fn quartiles_exclusive_and_inclusive() {
        let s: Vec<f64> = (1..=9).map(|x| x as f64).collect();
        assert_eq!(quantile(&s, 0.25, QuartileMethod::Exclusive), Some(2.5));
        assert_eq!(quantile(&s, 0.75, QuartileMethod::Exclusive), Some(7.5));
        assert_eq!(quantile(&s, 0.5, QuartileMethod::Exclusive), Some(5.0));
        assert_eq!(quantile(&s, 0.25, QuartileMethod::Inclusive), Some(3.0));
        assert_eq!(quantile(&s, 0.75, QuartileMethod::Inclusive), Some(7.0));
    }

    #[test]
    fn box_stats_detects_outlier() {
        let b = box_stats(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 100.0], QuartileMethod::Exclusive).unwrap();
        assert_eq!(b.outliers, vec![100.0]);
        assert_eq!(b.upper_whisker, 9.0);
        assert_eq!(b.lower_whisker, 1.0);
        assert_eq!(b.n, 10);
    }

    #[test]
    fn empty_and_nan_inputs() {
        assert!(box_stats(&[], QuartileMethod::Exclusive).is_none());
        assert!(histogram(&[f64::NAN], None, None, true).is_none());
        assert_eq!(sorted_finite(&[2.0, f64::NAN, 1.0]), vec![1.0, 2.0]);
    }
}
