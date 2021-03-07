use std::marker::PhantomData;
use std::alloc::Allocator;
use std::sync::Arc;
use std::any::Any;
use std::collections::BTreeMap;

use rendy_core::hal;
use hal::window::PresentationSurface;

use cranelift_entity::{PrimaryMap, SecondaryMap, entity_impl};

use crate::scheduler::{
    Scheduler, ScheduleEntry,
    builder::{ProceduralBuilder},
    interface::{GraphCtx, EntityId, SemaphoreId, ImageId},
};
use crate::factory::Factory;
use crate::command::RenderPassEncoder;
use crate::exec::ExecCtx;
use crate::command::Queue;

use crate::parameter::{ParameterStore, Parameter};

use crate::graph_borrowable::{GraphBorrowable, GraphBorrow, DynGraphBorrow};

mod macros;

pub mod unsafe_bump;
use unsafe_bump::Bump;

mod context;
pub use context::{GraphConstructCtx, PassConstructCtx, StandaloneConstructCtx};

pub struct GfxSchedulerTypes<B: hal::Backend>(PhantomData<B>);
impl<B: hal::Backend> crate::scheduler::SchedulerTypes for GfxSchedulerTypes<B> {
    type Image = GraphImage<B>;
    type Buffer = B::Buffer;
    type Semaphore = B::Semaphore;

    // TODO blocked by:
    // https://github.com/rust-lang/rust/issues/78459 :(
    //type NodeValue = Callback<B, Bump>;
    type NodeValue = Callback<B, std::alloc::Global>;
}

pub enum GraphImage<B: hal::Backend> {
    Image(B::Image),
    SwapchainImage(<B::Surface as PresentationSurface<B>>::SwapchainImage),
}

pub enum Callback<B: hal::Backend, A: Allocator> {
    None,
    //Standalone(Box<dyn FnOnce(&mut dyn Any, &mut Factory<B>, &mut RenderPassEncoder<B>), A>),
    Standalone(GraphGenerationNodeId, Box<dyn FnOnce(&mut dyn Any, &Factory<B>, &mut ExecCtx<B>, &mut Queue<B>) + 'static, A>),
    Pass(GraphGenerationNodeId, Box<(), A>),
}
impl<B: hal::Backend, A: Allocator> Default for Callback<B, A> {
    fn default() -> Self {
        Callback::None
    }
}

pub trait Node<B: hal::Backend>: 'static {
    type Argument;
    type Result;

    fn construct(
        &mut self,
        factory: &Factory<B>,
        ctx: &mut GraphConstructCtx<B>,
        arg: Self::Argument,
    ) -> Result<Self::Result, ()>;
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub struct GraphGenerationNodeId(u32);
entity_impl!(GraphGenerationNodeId);

pub struct Graph<'a, B: hal::Backend> {
    factory: &'a Factory<B>,

    procedural: ProceduralBuilder<GfxSchedulerTypes<B>>,
    nodes: PrimaryMap<GraphGenerationNodeId, DynGraphBorrow>,

    presents: BTreeMap<SemaphoreId, (GraphBorrow<rendy_wsi::Surface<B>>, ImageId, Box<dyn FnOnce(&mut dyn Any, Result<Option<hal::window::Suboptimal>, hal::window::PresentError>)>)>,

    /// A bump allocator for the current Graph generation.
    /// At the end of a generation, this will be cleared,
    /// meaning we NEED to make sure everything allocated in here
    /// is dropped before then.
    ///
    /// TODO: Usage is blocked by
    /// https://github.com/rust-lang/rust/issues/78459 :(
    generation_alloc: Bump,
}

impl<'a, B: hal::Backend> Graph<'a, B> {

    pub fn new(factory: &'a Factory<B>) -> Self {
        Self {
            factory,
            procedural: ProceduralBuilder::new(),
            nodes: PrimaryMap::new(),
            presents: BTreeMap::new(),
            generation_alloc: Bump::new(),
        }
    }

    pub fn construct_node<N: Node<B>>(&mut self, node: &mut GraphBorrowable<N>, argument: N::Argument) -> N::Result {
        let factory = &*self.factory;

        let mut node = node.take_borrow();

        let mut ctx = GraphConstructCtx {
            node_id: self.nodes.next_key(),
            inner: self,
        };
        let result = node.construct(factory, &mut ctx, argument).unwrap();

        let node_id = self.nodes.push(node.into_any());

        result
    }

    fn commit_standalone<F>(&mut self, node: GraphGenerationNodeId, exec: F)
    where
        F: FnOnce(&mut dyn Any, &Factory<B>, &mut ExecCtx<B>, &mut Queue<B>) + 'static,
    {
        self.procedural.commit(Callback::Standalone(node, Box::new(exec)));
    }

    fn add_present<F>(&mut self, surface: GraphBorrow<rendy_wsi::Surface<B>>, image: ImageId, result_handler: F)
    where
        F: FnOnce(&mut dyn Any, Result<Option<hal::window::Suboptimal>, hal::window::PresentError>) + 'static,
    {
        let sync_point = self.procedural.sync_point_get(image);
        let semaphore_id = self.procedural.sync_point_to_semaphore(sync_point);
        self.presents.insert(semaphore_id, (surface, image, Box::new(result_handler)));
    }

    pub fn schedule(&mut self, pool: &mut B::CommandPool, queue: &mut Queue<B>) {
        use hal::pool::CommandPool;
        use hal::command::CommandBuffer;

        self.procedural.postprocess();
        //let scheduler_input = self.procedural.make_scheduler_input();

        let mut scheduler = Scheduler::new();
        scheduler.plan(&self.procedural);

        for (schedule_idx, schedule_entry) in scheduler.scheduled_order.iter().enumerate() {
            let mut command_buffer = unsafe { pool.allocate_one(hal::command::Level::Primary) };
            unsafe {
                command_buffer.begin(
                    hal::command::CommandBufferFlags::ONE_TIME_SUBMIT,
                    hal::command::CommandBufferInheritanceInfo::default()
                );
            }

            let sync_slot = &scheduler.sync_strategy.slots[schedule_idx];

            let mut exec_ctx = crate::exec::ExecCtx {
                phantom: PhantomData,
            };

            match schedule_entry {
                ScheduleEntry::General(entity_id) => {
                    let callback_enum = self.procedural.remove_data(*entity_id).unwrap();
                    if let Callback::Standalone(node_id, callback) = callback_enum {
                        callback(&mut *self.nodes[node_id], &*self.factory, &mut exec_ctx, queue);
                    } else {
                        panic!()
                    }
                },
                ScheduleEntry::PassEntity(entity_id, render_pass) => todo!(),
            }
        }
    }

    fn reset_generation(&mut self) {

        // This MUST be called AFTER all allocated values are dropped.
        unsafe {
            self.generation_alloc.reset();
        }
    }

}
