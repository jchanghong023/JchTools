# Math routines

Mostly fast and accurate math.

Most of the methods have ULP less than 0.5.

本地修复：`f_pow` 的零指数与单位底数恒等式先于 NaN 传播；`f_powf` 对负零底数按指数奇偶性保留零或无穷的符号，与标准 `powf` 一致。对应回归为 `src/pow.rs` 的 `f_pow_nan_identity_and_special_value_controls` 和 `src/powf.rs` 的 `f_powf_signed_zero_controls`（覆盖运行期派发与通用后端）。
