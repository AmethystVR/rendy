use std::collections::BTreeSet;

use cranelift_entity::{PrimaryMap, SecondaryMap, EntityList, entity_impl};
use cranelift_entity_set::{EntitySetPool, EntitySet};

use super::{Scheduler, Direction, EntityKind, Entity, Resource, RenderPassData, SchedulerInput, hal};

impl Scheduler {

    pub(super) fn identify_render_passes(&mut self, builder: &SchedulerInput<(), ()>) {

        // Temporary pool for data within the computation
        let mut set_pool: EntitySetPool<Entity> = EntitySetPool::new();

        #[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
        struct RenderPassT(u32);
        entity_impl!(RenderPassT);

        struct RenderPassTData {
            entities: EntitySet<Entity>,
            for_cum: EntitySet<Entity>,
            rev_cum: EntitySet<Entity>,
        }

        // Temporary data
        let mut active_passes: BTreeSet<RenderPassT> = BTreeSet::new();
        let mut passes: PrimaryMap<RenderPassT, RenderPassTData> = PrimaryMap::new();
        let mut passes_back: SecondaryMap<Entity, Option<RenderPassT>> = SecondaryMap::with_default(None);

        // Temporary buffers
        let mut span_set = BTreeSet::new();
        let mut attachments_set = BTreeSet::new();

        // TODO TODO FIXME: We need to combine cumulative dependencies!!

        for span in builder.render_pass_spans.iter() {
            span_set.clear();

            let from = span.from();
            let to = span.to();

            println!(">> SPAN: {} -> {}", from, to);

            let mut from_for_cum = self.for_cum_deps[from].make_copy(&mut self.entity_set_pool);
            let mut to_rev_cum = self.rev_cum_deps[to].make_copy(&mut self.entity_set_pool);

            println!("FFC {:?}", from_for_cum.bind(&self.entity_set_pool));
            println!("TRC {:?}", to_rev_cum.bind(&self.entity_set_pool));

            if !from_for_cum.contains(from, &self.entity_set_pool) {
                // Entities share no dependencies!
                // TODO: Graph validation error?
                panic!()
            }

            // Query the entities that need to be scheduled between from and to.
            // This will iterate over the set of entities that need to be
            // scheduled between our two endpoints in order for all dependencies
            // to be satisfied.
            // `span_set` will contain the set of entities that need to be
            // scheduled between our two endpoints in order for all dependencies
            // to be satisfied.
            let required_entities_iter = from_for_cum.intersection(&to_rev_cum, &self.entity_set_pool);

            // Find all previously allocated render passes within our span set.
            // There are merged with the current.
            for entity in required_entities_iter {
                span_set.insert(entity);

                if let Some(old_pass) = passes_back[entity] {
                    let old_pass_data = &passes[old_pass];
                    span_set.extend(old_pass_data.entities.iter(&set_pool));
                }
            }

            for entity in span_set.iter() {
                if let Some(old_pass) = passes_back[*entity] {
                    let old_pass_data = &passes[old_pass];

                    from_for_cum.union_into(&old_pass_data.for_cum, &mut self.entity_set_pool);
                    to_rev_cum.union_into(&old_pass_data.rev_cum, &mut self.entity_set_pool);
                }
            }

            println!("span_set: {:?}", span_set);

            #[cfg(debug_assertions)]
            {
                // Sanity check.
                // Validate that the cumulative dependency query between the
                // first and last entity in our `span_set` is equal to our
                // `span_set`.

                let first = self.resource_schedule.first_entity(span_set.iter().cloned(), Direction::Forward).unwrap();
                let last = self.resource_schedule.first_entity(span_set.iter().cloned(), Direction::Reverse).unwrap();

                let mut num = 0;
                for q_entity in self.for_cum_deps[first].intersection(&self.rev_cum_deps[last], &self.entity_set_pool) {
                    assert!(span_set.contains(&q_entity));
                    num += 1;
                }
                println!("span_set: {}  num: {}", span_set.len(), num);
                assert!(span_set.len() == num);

            }

            // The render pass merge is ONLY valid if:
            // * All entities in merge are valid entities within a render pass.
            // * There are no resources that are both uses and attachments.
            //
            // Validate this. If validation fails, emit a graph warning and skip
            // this span.

            // Generate set of resources used as attachments
            for entity in span_set.iter() {
                for (resource, aux) in self.resource_schedule.usages_by(*entity) {
                    if aux.usage_kind.is_attachment() {
                        attachments_set.insert(resource);
                    }
                }
            }

            // Validate that no attachments are used as uses
            for entity in span_set.iter() {
                for (resource, aux) in self.resource_schedule.usages_by(*entity) {
                    if !aux.usage_kind.is_attachment() {
                        // TODO emit graph warning and bail
                        assert!(!attachments_set.contains(&resource));
                    }
                }
                // TODO emit graph warning and bail
                assert!(builder.entity[*entity].kind == EntityKind::Pass);
            }

            // Create new pass set and replace entities
            let new_pass = passes.next_key();
            let mut new_set = EntitySet::new();
            for entity in span_set.iter() {
                if let Some(prev) = passes_back[*entity] {
                    active_passes.remove(&prev);
                }
                passes_back[*entity] = Some(new_pass);
                new_set.insert(*entity, &mut set_pool);
            }
            let new_pass_k = passes.push(RenderPassTData {
                entities: new_set,
                for_cum: from_for_cum,
                rev_cum: to_rev_cum,
            });
            debug_assert!(new_pass_k == new_pass);
            active_passes.insert(new_pass);

        }

        // Generate real pass order and make the pass description
        span_set.clear();
        for pass in active_passes.iter() {
            let set = &passes[*pass].entities.bind(&set_pool);

            let mut entities = EntityList::new();
            let mut attachments = EntitySet::new();
            let mut uses = EntitySet::new();
            let mut writes = EntitySet::new();

            let mut for_cum_merge = EntitySet::new();
            let mut rev_cum_merge = EntitySet::new();

            let n_pass = self.passes.next_key();

            for entity in self.resource_schedule.iter_in_order_boundset(&set) {
                // Push to entities list
                entities.push(entity, &mut self.entity_list_pool);
                // Update which pass the entity belongs to
                self.passes_back[entity] = Some(n_pass);

                #[cfg(debug_assertions)]
                {
                    // Sanity check.
                    // Validate that the entity only belongs to one pass.
                    assert!(!span_set.contains(&entity));
                    span_set.insert(entity);
                }

                // Update usage sets
                for (resource, aux) in self.resource_schedule.usages_by(entity) {
                    if aux.usage_kind.is_attachment() {
                        attachments.insert(resource, &mut self.resource_set_pool);
                    } else {
                        uses.insert(resource, &mut self.resource_set_pool);
                    }
                    if aux.usage_kind.is_write() {
                        writes.insert(resource, &mut self.resource_set_pool);
                    }
                }

                // Update cumulative sets
                for_cum_merge.union_into(&self.for_cum_deps[entity], &mut self.entity_set_pool);
                rev_cum_merge.union_into(&self.rev_cum_deps[entity], &mut self.entity_set_pool);
            }

            // TODO possibly add start and end cumulative dependency sets to
            // render pass data struct?

            // Add pass and mark as active
            self.passes.push(RenderPassData {
                entities,
                members: passes[*pass].entities.make_copy_other(&set_pool, &mut self.entity_set_pool),

                attachments,
                uses,
                writes,

                for_cum: for_cum_merge,
                rev_cum: rev_cum_merge,
            });
            self.active_passes.insert(n_pass);
        }

        // Allocate every relevant entity that is not in a pass already in its
        // own one?
        // TODO TODO FIXME

    }

}
