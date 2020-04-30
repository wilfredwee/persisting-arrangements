use std::io::{Read, Write};

use abomonation::{decode, encode};
use differential_dataflow::input::Input;
use differential_dataflow::operators::arrange::{ArrangeByKey, ArrangeBySelf};
use differential_dataflow::operators::consolidate::Consolidate;
use differential_dataflow::operators::JoinCore;
use differential_dataflow::trace::implementations::ord::OrdValBatch;
use differential_dataflow::trace::{BatchReader, Cursor};
use differential_dataflow::AsCollection;
use timely::dataflow::channels::pact::Pipeline;
use timely::dataflow::operators::generic::operator::Operator;
use timely::dataflow::operators::generic::source;
use timely::dataflow::operators::probe::{Handle, Probe};
use timely::dataflow::operators::{Capability, Inspect};
use timely::order::PartialOrder;
use timely::scheduling::Scheduler;

//
// Open questions
// 1. Why does the batch have an upper of []?
// 2. How to write down the batch in a sink in a format we can read back in?
// 3. How do we read the batch data back in
//  a. it seems like at least in this toy we could interleave a timely source with some control flow
//  but its a scattered thought

fn main() {
    // define a new timely dataflow computation.
    timely::execute_from_args(std::env::args(), move |worker| {
        let mut probe = timely::dataflow::operators::probe::Handle::new();

        let mut old_batch = worker.dataflow::<i32, _, _>(|scope| {
            source::<_, _, _, _>(scope, "BatchReader", |mut capability, info| {
                let activator = scope.activator_for(&info.address[..]);

                let mut cap = Some(capability);
                move |output| {
                    if let Some(cap) = cap.as_mut() {
                        let mut time = cap.time().clone();
                        {
                            // get some data and send it.
                            let mut file =
                                std::fs::File::open("testing-3").expect("open stored batch file");
                            let mut buf: Vec<u8> = Vec::new();
                            file.read_to_end(&mut buf).expect("read failed");

                            // TODO unclear how completely we need to specify types
                            // for OrdValBatch here
                            let batch = if let Some((batch, remaining)) =
                                unsafe { decode::<OrdValBatch<i32, i32, i32, isize>>(&mut buf) }
                            {
                                assert!(remaining.len() == 0);
                                //println!("decoded: {:?}", batch);
                                batch
                            } else {
                                // TODO In the future we'll need to be able to handle this
                                // more gracefully
                                panic!("unable to decode batch");
                            };

                            let mut session = output.session(&cap);
                            // Step through all the (key, val, time, diff) tuples in this batch
                            // and send them to our computation
                            let mut cursor = batch.cursor();
                            cursor.rewind_keys(&batch);
                            cursor.rewind_vals(&batch);

                            while cursor.key_valid(&batch) {
                                let key = cursor.key(&batch);
                                while cursor.val_valid(&batch) {
                                    let val = cursor.val(&batch);
                                    cursor.map_times(&batch, |ts, r| {
                                        if time.less_than(ts) {
                                            time = *ts;
                                        }
                                        session.give((
                                            (key.clone(), val.clone()),
                                            // XXX this is a total hack
                                            // but the intuition is - if updates can be brought
                                            // forward to a time on the frontier
                                            // why can't they go backwards? (before any execution)
                                            // Make it so that all updates from saved state present as happening
                                            // at Timestamp::minimum() TODO actually use that API
                                            // The goal is to let us use a saved arrangement without having
                                            // to coordinate with other dataflows about which timestamp to go
                                            // forwards to
                                            // I think we may be able to achieve a similar transform via going forwards
                                            // in time with delay
                                            0,
                                            r.clone(),
                                        ));
                                    });
                                    cursor.step_val(&batch);
                                }
                                cursor.step_key(&batch);
                            }
                        }
                        cap.downgrade(&time);
                    }
                    // downgrade capability.
                    activator.activate();
                    cap = None;
                }
            })
            .probe_with(&mut probe)
            .as_collection()
            //.consolidate()
            .inspect(|x| println!("rereading {:?}", x))
            .arrange_by_key()
            .trace
        });

        // define a new computation.
        let mut input = worker.dataflow(|scope| {
            let (handle, manages) = scope.new_collection();
            manages.inspect(|x| println!("manages: {:?}", x));
            let old = old_batch.import(scope);
            let reverse = manages.map(|(manager, employee)| (employee, manager));

            // Let's store and bring back these collections because we have a good idea
            // how they evolve
            let manages = manages.arrange_by_key();
            let reverse = reverse.arrange_by_key();

            // if (m2, m1) and (m1, p), then output (m1, (m2, p))
            manages
                .join_core(&old, |m2, m1, p| Some((*m2, *m1, *p)))
                .inspect(|x| println!("join: {:?}", x));

            // We'll use a sink to write down the stream of batches we
            // get from the manages collection
            manages.stream.sink(Pipeline, "BatchWriter", |input| {
                while let Some((_time, data)) = input.next() {
                    for d in data.iter() {
                        let mut bytes = Vec::new();
                        unsafe {
                            encode(&(**d), &mut bytes).expect("encoding batches failed");
                        }
                        println!("{:?}", d);
                        println!("{:?}", bytes);

                        // TODO in a more realistic scenario we would give this file a new name every time
                        let mut file = std::fs::File::create("testing-end")
                            .expect("creating file to write batches");
                        file.write(&bytes).expect("writing batches should succeed");
                        file.flush().expect("flushing batches should succeed");
                    }
                }
            });

            // return the handle so other non-dataflow code can feed us data
            handle
        });

        // Read a size for our organization from the arguments.
        let size: i32 = std::env::args().nth(1).unwrap().parse().unwrap();

        // Load input (a binary tree).
        input.advance_to(0);

        for person in 0..size {
            input.insert((person / 2, person));
        }

        for person in 1..size {
            input.advance_to(person);
            input.remove((person / 2, person));
            input.insert((person / 3, person));
        }
    })
    .expect("Computation terminated abnormally");
}
