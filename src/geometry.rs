//! 3-D maths: vectors, rotations, small linear solvers and a symmetric eigen solver.

pub type V3 = [f64; 3];
pub type M3 = [[f64; 3]; 3];

pub const IDENTITY: M3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

#[inline]
pub fn dot(a: V3, b: V3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
#[inline]
pub fn cross(a: V3, b: V3) -> V3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}
#[inline]
pub fn norm(a: V3) -> f64 {
    dot(a, a).sqrt()
}
#[inline]
pub fn normalize(a: V3) -> V3 {
    let n = norm(a);
    if n < 1e-15 { a } else { [a[0] / n, a[1] / n, a[2] / n] }
}
#[inline]
pub fn mv(m: &M3, v: V3) -> V3 {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}
/// transpose(m) * v
#[inline]
pub fn mtv(m: &M3, v: V3) -> V3 {
    [
        m[0][0] * v[0] + m[1][0] * v[1] + m[2][0] * v[2],
        m[0][1] * v[0] + m[1][1] * v[1] + m[2][1] * v[2],
        m[0][2] * v[0] + m[1][2] * v[1] + m[2][2] * v[2],
    ]
}
pub fn mm(a: &M3, b: &M3) -> M3 {
    let mut r = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            r[i][j] = a[i][0] * b[0][j] + a[i][1] * b[1][j] + a[i][2] * b[2][j];
        }
    }
    r
}
pub fn transpose(a: &M3) -> M3 {
    let mut r = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            r[i][j] = a[j][i];
        }
    }
    r
}

/// Rotation matrix from axis-angle vector (Rodrigues).
pub fn rodrigues(w: V3) -> M3 {
    let th = norm(w);
    if th < 1e-12 {
        return [[1.0, -w[2], w[1]], [w[2], 1.0, -w[0]], [-w[1], w[0], 1.0]];
    }
    let k = [w[0] / th, w[1] / th, w[2] / th];
    let (s, c) = th.sin_cos();
    let v = 1.0 - c;
    [
        [c + k[0] * k[0] * v, k[0] * k[1] * v - k[2] * s, k[0] * k[2] * v + k[1] * s],
        [k[1] * k[0] * v + k[2] * s, c + k[1] * k[1] * v, k[1] * k[2] * v - k[0] * s],
        [k[2] * k[0] * v - k[1] * s, k[2] * k[1] * v + k[0] * s, c + k[2] * k[2] * v],
    ]
}

pub fn rot_x(a: f64) -> M3 {
    let (s, c) = a.sin_cos();
    [[1.0, 0.0, 0.0], [0.0, c, -s], [0.0, s, c]]
}
pub fn rot_y(a: f64) -> M3 {
    let (s, c) = a.sin_cos();
    [[c, 0.0, s], [0.0, 1.0, 0.0], [-s, 0.0, c]]
}
pub fn rot_z(a: f64) -> M3 {
    let (s, c) = a.sin_cos();
    [[c, -s, 0.0], [s, c, 0.0], [0.0, 0.0, 1.0]]
}

/// World->camera rotation for a camera looking at yaw (right positive), pitch (up positive), roll.
/// Coordinates: x right, y down, z forward.
pub fn camera_from_ypr(yaw: f64, pitch: f64, roll: f64) -> M3 {
    // camera-to-world = Ry(yaw) * Rx(pitch) * Rz(roll); world-to-camera = transpose.
    let c2w = mm(&mm(&rot_y(yaw), &rot_x(pitch)), &rot_z(roll));
    transpose(&c2w)
}

/// Yaw, pitch, roll (radians) of a world->camera rotation.
pub fn ypr_of(r: &M3) -> (f64, f64, f64) {
    let c2w = transpose(r);
    let fwd = [c2w[0][2], c2w[1][2], c2w[2][2]];
    let yaw = fwd[0].atan2(fwd[2]);
    let pitch = (-fwd[1]).clamp(-1.0, 1.0).asin();
    // remove yaw & pitch, remaining is roll around z
    let base = mm(&rot_y(yaw), &rot_x(pitch));
    let rz = mm(&transpose(&base), &c2w);
    let roll = rz[1][0].atan2(rz[0][0]);
    (yaw, pitch, roll)
}

/// Orthonormalise a rotation matrix (Gram-Schmidt on rows).
pub fn orthonormalize(r: &M3) -> M3 {
    let x = normalize(r[0]);
    let mut y = r[1];
    let d = dot(x, y);
    y = normalize([y[0] - d * x[0], y[1] - d * x[1], y[2] - d * x[2]]);
    let z = cross(x, y);
    [x, y, z]
}

/// Jacobi eigen decomposition of a symmetric n x n matrix.
/// Returns (eigenvalues, eigenvectors as columns: vecs[row][col]).
pub fn jacobi_eigen(a: &[Vec<f64>]) -> (Vec<f64>, Vec<Vec<f64>>) {
    let n = a.len();
    let mut a: Vec<Vec<f64>> = a.to_vec();
    let mut v = vec![vec![0.0; n]; n];
    for (i, row) in v.iter_mut().enumerate() {
        row[i] = 1.0;
    }
    for _sweep in 0..60 {
        let mut off = 0.0;
        for i in 0..n {
            for j in i + 1..n {
                off += a[i][j] * a[i][j];
            }
        }
        if off < 1e-22 {
            break;
        }
        for p in 0..n {
            for q in p + 1..n {
                if a[p][q].abs() < 1e-300 {
                    continue;
                }
                let theta = (a[q][q] - a[p][p]) / (2.0 * a[p][q]);
                let t = theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt());
                let t = if theta == 0.0 { 1.0 } else { t };
                let c = 1.0 / (t * t + 1.0).sqrt();
                let s = t * c;
                for k in 0..n {
                    let akp = a[k][p];
                    let akq = a[k][q];
                    a[k][p] = c * akp - s * akq;
                    a[k][q] = s * akp + c * akq;
                }
                for k in 0..n {
                    let apk = a[p][k];
                    let aqk = a[q][k];
                    a[p][k] = c * apk - s * aqk;
                    a[q][k] = s * apk + c * aqk;
                }
                for k in 0..n {
                    let vkp = v[k][p];
                    let vkq = v[k][q];
                    v[k][p] = c * vkp - s * vkq;
                    v[k][q] = s * vkp + c * vkq;
                }
            }
        }
    }
    ((0..n).map(|i| a[i][i]).collect(), v)
}

/// Best rotation R with b ≈ R a (Horn's quaternion method), weights optional.
pub fn horn_rotation(pairs: &[(V3, V3)]) -> M3 {
    let mut s = [[0.0f64; 3]; 3];
    for (a, b) in pairs {
        for i in 0..3 {
            for j in 0..3 {
                s[i][j] += a[i] * b[j];
            }
        }
    }
    let (sxx, sxy, sxz) = (s[0][0], s[0][1], s[0][2]);
    let (syx, syy, syz) = (s[1][0], s[1][1], s[1][2]);
    let (szx, szy, szz) = (s[2][0], s[2][1], s[2][2]);
    let n = vec![
        vec![sxx + syy + szz, syz - szy, szx - sxz, sxy - syx],
        vec![syz - szy, sxx - syy - szz, sxy + syx, szx + sxz],
        vec![szx - sxz, sxy + syx, -sxx + syy - szz, syz + szy],
        vec![sxy - syx, szx + sxz, syz + szy, -sxx - syy + szz],
    ];
    let (vals, vecs) = jacobi_eigen(&n);
    let mut best = 0;
    for i in 1..4 {
        if vals[i] > vals[best] {
            best = i;
        }
    }
    let q = [vecs[0][best], vecs[1][best], vecs[2][best], vecs[3][best]];
    let (w, x, y, z) = (q[0], q[1], q[2], q[3]);
    [
        [w * w + x * x - y * y - z * z, 2.0 * (x * y - w * z), 2.0 * (x * z + w * y)],
        [2.0 * (y * x + w * z), w * w - x * x + y * y - z * z, 2.0 * (y * z - w * x)],
        [2.0 * (z * x - w * y), 2.0 * (z * y + w * x), w * w - x * x - y * y + z * z],
    ]
}

/// Solve a dense linear system in place (Gaussian elimination with partial pivoting).
pub fn solve(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Option<Vec<f64>> {
    let n = b.len();
    for c in 0..n {
        let mut p = c;
        for r in c + 1..n {
            if a[r][c].abs() > a[p][c].abs() {
                p = r;
            }
        }
        if a[p][c].abs() < 1e-14 {
            return None;
        }
        a.swap(c, p);
        b.swap(c, p);
        for r in c + 1..n {
            let f = a[r][c] / a[c][c];
            if f == 0.0 {
                continue;
            }
            for k in c..n {
                a[r][k] -= f * a[c][k];
            }
            b[r] -= f * b[c];
        }
    }
    let mut x = vec![0.0; n];
    for r in (0..n).rev() {
        let mut s = b[r];
        for k in r + 1..n {
            s -= a[r][k] * x[k];
        }
        x[r] = s / a[r][r];
    }
    Some(x)
}

/// Apply a 3x3 homography to a 2-D point.
#[inline]
pub fn h_apply(h: &[f64; 9], x: f64, y: f64) -> (f64, f64) {
    let w = h[6] * x + h[7] * y + h[8];
    ((h[0] * x + h[1] * y + h[2]) / w, (h[3] * x + h[4] * y + h[5]) / w)
}

/// Least-squares homography from >= 4 point pairs (DLT with h33 = 1, normalised).
pub fn fit_homography(src: &[(f64, f64)], dst: &[(f64, f64)]) -> Option<[f64; 9]> {
    if src.len() < 4 {
        return None;
    }
    let norm_t = |pts: &[(f64, f64)]| -> (f64, f64, f64) {
        let n = pts.len() as f64;
        let mx = pts.iter().map(|p| p.0).sum::<f64>() / n;
        let my = pts.iter().map(|p| p.1).sum::<f64>() / n;
        let d = pts.iter().map(|p| ((p.0 - mx).powi(2) + (p.1 - my).powi(2)).sqrt()).sum::<f64>() / n;
        let s = if d > 1e-12 { std::f64::consts::SQRT_2 / d } else { 1.0 };
        (mx, my, s)
    };
    let (sx, sy, ss) = norm_t(src);
    let (dx, dy, ds) = norm_t(dst);
    let mut ata = vec![vec![0.0; 8]; 8];
    let mut atb = vec![0.0; 8];
    for (p, q) in src.iter().zip(dst) {
        let x = (p.0 - sx) * ss;
        let y = (p.1 - sy) * ss;
        let u = (q.0 - dx) * ds;
        let v = (q.1 - dy) * ds;
        let r1 = [x, y, 1.0, 0.0, 0.0, 0.0, -u * x, -u * y];
        let r2 = [0.0, 0.0, 0.0, x, y, 1.0, -v * x, -v * y];
        for (row, rhs) in [(r1, u), (r2, v)] {
            for i in 0..8 {
                atb[i] += row[i] * rhs;
                for j in 0..8 {
                    ata[i][j] += row[i] * row[j];
                }
            }
        }
    }
    let h = solve(ata, atb)?;
    let hn = [h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7], 1.0];
    // denormalise: H = Td^-1 * Hn * Ts
    let ts = [ss, 0.0, -ss * sx, 0.0, ss, -ss * sy, 0.0, 0.0, 1.0];
    let tdi = [1.0 / ds, 0.0, dx, 0.0, 1.0 / ds, dy, 0.0, 0.0, 1.0];
    let m = mul33(&tdi, &mul33(&hn, &ts));
    if m[8].abs() < 1e-15 {
        return None;
    }
    let mut out = [0.0; 9];
    for i in 0..9 {
        out[i] = m[i] / m[8];
    }
    Some(out)
}

pub fn mul33(a: &[f64; 9], b: &[f64; 9]) -> [f64; 9] {
    let mut r = [0.0; 9];
    for i in 0..3 {
        for j in 0..3 {
            r[i * 3 + j] = a[i * 3] * b[j] + a[i * 3 + 1] * b[3 + j] + a[i * 3 + 2] * b[6 + j];
        }
    }
    r
}

pub fn inv33(m: &[f64; 9]) -> Option<[f64; 9]> {
    let det = m[0] * (m[4] * m[8] - m[5] * m[7]) - m[1] * (m[3] * m[8] - m[5] * m[6])
        + m[2] * (m[3] * m[7] - m[4] * m[6]);
    if det.abs() < 1e-15 {
        return None;
    }
    let id = 1.0 / det;
    Some([
        (m[4] * m[8] - m[5] * m[7]) * id,
        (m[2] * m[7] - m[1] * m[8]) * id,
        (m[1] * m[5] - m[2] * m[4]) * id,
        (m[5] * m[6] - m[3] * m[8]) * id,
        (m[0] * m[8] - m[2] * m[6]) * id,
        (m[2] * m[3] - m[0] * m[5]) * id,
        (m[3] * m[7] - m[4] * m[6]) * id,
        (m[1] * m[6] - m[0] * m[7]) * id,
        (m[0] * m[4] - m[1] * m[3]) * id,
    ])
}

/// Tiny deterministic random generator (xorshift).
pub struct Rng(pub u64);
impl Rng {
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn gauss(&mut self) -> f64 {
        let u1 = self.unit().max(1e-12);
        let u2 = self.unit();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}
