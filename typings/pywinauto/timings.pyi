# pywinauto.timings 的最小类型桩。

# wait()/wait_until() 等待条件超时后抛出；与内建 TimeoutError 无继承关系。
class TimeoutError(RuntimeError): ...
