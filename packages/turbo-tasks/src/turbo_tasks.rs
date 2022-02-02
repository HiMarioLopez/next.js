use std::{
    any::{Any, TypeId},
    cell::Cell,
    future::Future,
    hash::Hash,
    pin::Pin,
    sync::Arc,
};

use any_key::AnyHash;
use anyhow::{anyhow, Result};
use async_std::{
    task::{Builder, JoinHandle},
    task_local,
};
use chashmap::CHashMap;

use crate::{task::NativeTaskFuture, viz::Visualizable, NativeFunction, NodeRef, Task};

pub struct TurboTasks {
    interning_map: CHashMap<Box<dyn AnyHash + Send + Sync>, NodeRef>,
    task_cache: CHashMap<(&'static NativeFunction, Vec<NodeRef>), Arc<Task>>,
}

task_local! {
    static TURBO_TASKS: Cell<Option<&'static TurboTasks>> = Cell::new(None);
}

impl TurboTasks {
    // TODO better lifetime management for turbo tasks
    // consider using unsafe for the task_local turbo tasks
    // that should be safe as long tasks can't outlife turbo task
    // so we probably want to make sure that all tasks are joined
    // when trying to drop turbo tasks
    pub fn new() -> &'static Self {
        Box::leak(Box::new(Self {
            interning_map: CHashMap::new(),
            task_cache: CHashMap::new(),
        }))
    }

    pub fn spawn_root_task(
        &'static self,
        functor: impl Fn() -> NativeTaskFuture + Sync + Send + 'static,
    ) -> Arc<Task> {
        let task = Arc::new(Task::new_root(functor));
        self.schedule(task.clone());
        task
    }

    pub fn dynamic_call(
        self: &'static TurboTasks,
        func: &'static NativeFunction,
        inputs: Vec<NodeRef>,
    ) -> Result<Pin<Box<dyn Future<Output = Option<NodeRef>> + Sync + Send>>> {
        let mut result_task = Err(anyhow!("Unreachable"));
        self.task_cache
            .alter((func, inputs.clone()), |old| match old {
                Some(t) => {
                    result_task = Ok(t.clone());
                    Some(t)
                }
                None => match Task::new_native(inputs, func) {
                    Ok(task) => {
                        let new_task = Arc::new(task);
                        self.schedule(new_task.clone());
                        result_task = Ok(new_task.clone());
                        Some(new_task)
                    }
                    Err(err) => {
                        result_task = Err(err);
                        None
                    }
                },
            });
        let task = result_task?;
        task.ensure_scheduled(self);
        return Ok(Box::pin(task.into_output(self)));
    }

    pub(crate) fn schedule(&'static self, task: Arc<Task>) -> JoinHandle<()> {
        Builder::new()
            .name(format!("{:?} {:?}", &*task, &*task as *const Task))
            .spawn(async move {
                Task::set_current(task.clone());
                TURBO_TASKS.with(|c| c.set(Some(self)));
                task.execution_started();
                let result = task.execute().await;
                task.finalize_execution();
                task.execution_completed(result, self);
            })
            .unwrap()
    }

    pub(crate) fn current() -> Option<&'static Self> {
        TURBO_TASKS.with(|c| c.get())
    }

    pub(crate) fn intern<
        T: Any + ?Sized,
        K: Hash + PartialEq + Eq + Send + Sync + 'static,
        F: FnOnce() -> NodeRef,
    >(
        &self,
        key: K,
        fallback: F,
    ) -> NodeRef {
        let mut node1 = None;
        let mut node2 = None;
        self.interning_map.upsert(
            Box::new((TypeId::of::<T>(), key)) as Box<dyn AnyHash + Send + Sync>,
            || {
                let new_node = fallback();
                node1 = Some(new_node.clone());
                new_node
            },
            |existing_node| {
                node2 = Some(existing_node.clone());
            },
        );
        // TODO ugly
        if let Some(n) = node1 {
            return n;
        }
        node2.unwrap()
    }
}

pub fn dynamic_call(
    func: &'static NativeFunction,
    inputs: Vec<NodeRef>,
) -> Result<Pin<Box<dyn Future<Output = Option<NodeRef>> + Sync + Send>>> {
    let tt = TurboTasks::current()
        .ok_or_else(|| anyhow!("tried to call dynamic_call outside of turbo tasks"))?;
    tt.dynamic_call(func, inputs)
}

pub(crate) fn intern<
    T: Any + ?Sized,
    K: Hash + PartialEq + Eq + Send + Sync + 'static,
    F: FnOnce() -> NodeRef,
>(
    key: K,
    fallback: F,
) -> NodeRef {
    let tt = TurboTasks::current()
        .ok_or_else(|| anyhow!("tried to call intern outside of turbo tasks"))
        .unwrap();
    tt.intern::<T, K, F>(key, fallback)
}

impl Visualizable for &'static TurboTasks {
    fn visualize(&self, visualizer: &mut impl crate::viz::Visualizer) {
        for (key, task) in self.task_cache.clone().into_iter() {
            task.visualize(visualizer);
        }
    }
}
