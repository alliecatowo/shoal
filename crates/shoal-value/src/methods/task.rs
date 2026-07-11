//! Task lifecycle methods (defect #14, TDD §4.7 job control).

use super::*;

pub(crate) fn task_await(recv: Value) -> VResult<Value> {
    match recv {
        Value::Task(t) => t.wait(),
        v => Err(ErrorVal::type_error(format!(
            ".await expects a task, found {}",
            v.type_name()
        ))),
    }
}
pub(crate) fn task_cancel(recv: Value) -> VResult<Value> {
    match recv {
        Value::Task(t) => {
            t.cancel();
            Ok(Value::Null)
        }
        v => Err(ErrorVal::type_error(format!(
            ".cancel expects a task, found {}",
            v.type_name()
        ))),
    }
}
pub(crate) fn task_is_done(recv: Value) -> VResult<Value> {
    match recv {
        Value::Task(t) => Ok(Value::Bool(t.is_done())),
        v => Err(ErrorVal::type_error(format!(
            ".is_done expects a task, found {}",
            v.type_name()
        ))),
    }
}
pub(crate) fn task_suspend(recv: Value) -> VResult<Value> {
    match recv {
        Value::Task(t) => {
            t.suspend();
            Ok(Value::Task(t))
        }
        v => Err(ErrorVal::type_error(format!(
            ".suspend expects a task, found {}",
            v.type_name()
        ))),
    }
}
pub(crate) fn task_resume(recv: Value) -> VResult<Value> {
    match recv {
        Value::Task(t) => {
            t.resume();
            Ok(Value::Task(t))
        }
        v => Err(ErrorVal::type_error(format!(
            ".resume expects a task, found {}",
            v.type_name()
        ))),
    }
}
pub(crate) fn task_is_suspended(recv: Value) -> VResult<Value> {
    match recv {
        Value::Task(t) => Ok(Value::Bool(t.is_suspended())),
        v => Err(ErrorVal::type_error(format!(
            ".is_suspended expects a task, found {}",
            v.type_name()
        ))),
    }
}
