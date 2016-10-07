/* 
 * Copyright (c) 2016 Snowplow Analytics Ltd. All rights reserved.
 *
 * This program is licensed to you under the Apache License Version 2.0, and
 * you may not use this file except in compliance with the Apache License
 * Version 2.0.  You may obtain a copy of the Apache License Version 2.0 at
 * http://www.apache.org/licenses/LICENSE-2.0.
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the Apache License Version 2.0 is distributed on an "AS
 * IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or
 * implied.  See the Apache License Version 2.0 for the specific language
 * governing permissions and limitations there under.
 */
 
use factotum::factfile::Task as FactfileTask;
use factotum::factfile::Factfile;
use std::process::Command;
use std::thread;
use std::sync::mpsc;

pub mod execution_strategy;
pub mod task_list;
#[cfg(test)]
mod tests;

use factotum::executor::task_list::*;
use factotum::executor::execution_strategy::*;

pub fn get_task_execution_list(factfile:&Factfile, start_from:Option<String>) -> TaskList<&FactfileTask> {
    let mut task_list = TaskList::<&FactfileTask>::new();

    let tasks = if let Some(start_task) = start_from {
        info!("Reduced run! starting from {}", &start_task);
        factfile.get_tasks_in_order_from(&start_task)  
    } else {
        factfile.get_tasks_in_order()
    };
    
    for task_level in tasks.iter() {
        let task_group: TaskGroup<&FactfileTask> = task_level
                                                    .iter()
                                                    .map(|t| task_list::Task::<&FactfileTask>::new(t.name.clone(), t))
                                                    .collect();
        match task_list.add_group(task_group) {
            Ok(_) => (),
            Err(msg) => panic!(format!("Couldn't add task to group: {}", msg))
        }
    }

    for task_level in tasks.iter() {
        for task in task_level.iter() {
            for dep in task.depends_on.iter() {
                if task_list.is_task_name_present(&dep) && task_list.is_task_name_present(&task.name) {
                    match task_list.set_child(&dep, &task.name) {
                        Ok(_) => (),
                        Err(msg) => panic!(format!("Executor: couldn't add '{}' to child '{}': {}", dep, task.name, msg))
                    }
                }
            }
        }
    }

    task_list
}


pub type TaskSnapshot = Vec<Task<FactfileTask>>;

#[derive(Debug, PartialEq, Clone)]
pub enum ExecutionState {
    STARTED(TaskSnapshot),
    RUNNING(TaskSnapshot),
    FINISHED(TaskSnapshot)    
}

pub fn get_task_snapshot(tasklist: &TaskList<&FactfileTask>) -> TaskSnapshot {
    tasklist.tasks
            .iter()
            .flat_map(|task_grp| task_grp.iter().map(|task| Task { 
                                                                   name: task.name.clone(), 
                                                                   task_spec: task.task_spec.clone(), 
                                                                   state: task.state.clone(),
                                                                   run_result: task.run_result.clone() 
                                                                  } ))
            .collect()
}

pub fn execute_factfile<'a, F>(factfile:&'a Factfile, start_from:Option<String>, strategy:F, progress_channel:Option<mpsc::Sender<ExecutionState>>) -> TaskList<&'a FactfileTask>
    where F : Fn(&str, &mut Command) -> RunResult + Send + Sync + 'static + Copy {
    
    let mut tasklist = get_task_execution_list(factfile, start_from);

   // notify the progress channel
   if let Some(ref send) = progress_channel {
       send.send(ExecutionState::STARTED(get_task_snapshot(&tasklist))).unwrap();
   }
              
    for task_grp_idx in 0..tasklist.tasks.len() {
        // everything in a task "group" gets run together
        let (tx, rx) = mpsc::channel::<(usize, RunResult)>();
    
        {
            let ref mut task_group = tasklist.tasks[task_grp_idx];
            for (idx,task) in task_group.into_iter().enumerate() {
                info!("Running task '{}'!", task.name);
                task.state = State::RUNNING;
                {
                    let tx = tx.clone();
                    let args = format_args(&task.task_spec.command, &task.task_spec.arguments);
                    let task_name = task.name.to_string();
                    
                    thread::spawn(move || {
                        let mut command = Command::new("sh");
                        command.arg("-c");
                        command.arg(args);
                        let task_result = strategy(&task_name, &mut command);
                        tx.send((idx, task_result)).unwrap();
                    });  
                }          
            }    
        }
        
        if let Some(ref send) = progress_channel {
            send.send(ExecutionState::RUNNING(get_task_snapshot(&tasklist))).unwrap();
        }            
        
        let mut terminate_job_please = false; 
        let mut task_failed = false;
        
        for _ in 0..tasklist.tasks[task_grp_idx].len() {                    
            let (idx, task_result) = rx.recv().unwrap();                              
            info!("'{}' returned {} in {:?}", tasklist.tasks[task_grp_idx][idx].name, task_result.return_code, task_result.duration); 
                
            {
                let ref mut task_group = tasklist.tasks[task_grp_idx];
                if task_group[idx].task_spec.on_result.terminate_job.contains(&task_result.return_code) {
                    // if the return code is in the terminate early list, prune the sub-tree (set to skipped) return early term
                    task_group[idx].state = State::SUCCESS_NOOP;
                    terminate_job_please = true;
                } else if task_group[idx].task_spec.on_result.continue_job.contains(&task_result.return_code) {
                    // if the return code is in the continue list, return success
                    task_group[idx].state = State::SUCCESS;
                } else {
                    // if the return code is not in either list, prune the sub-tree (set to skipped) and return error
                    let expected_codes = task_group[idx].task_spec.on_result.continue_job.iter()
                                                                            .map(|code| code.to_string())
                                                                            .collect::<Vec<String>>()
                                                                            .join(",");
                    let err_msg = format!("the task exited with a value not specified in continue_job - {} (task expects one of the following return codes to continue [{}])", 
                                        task_result.return_code,
                                        expected_codes);
                    task_group[idx].state = State::FAILED(err_msg);
                    task_failed = true;
                }

                task_group[idx].run_result = Some(task_result);
            }

            if let Some(ref send) = progress_channel {
                send.send(ExecutionState::RUNNING(get_task_snapshot(&tasklist))).unwrap();
            }    

        }
        
        if terminate_job_please || task_failed {
            break;
        }

    }

    if let Some(ref send) = progress_channel {
        let mut snapshot = get_task_snapshot(&tasklist);
        for mut task in snapshot.iter_mut() {
            if State::WAITING == task.state {
                 task.state = State::SKIPPED("A prior failure or terminate request caused this task to be skipped".to_string());
            } 
        }
        send.send(ExecutionState::FINISHED(snapshot)).unwrap();
    }    

    tasklist 
}

pub fn format_args(command:&str, args:&Vec<String>) -> String {
    let arg_str = args.iter()
                      .map(|s| format!("\"{}\"", s))
                      .collect::<Vec<String>>()
                      .join(" ");
    format!("{} {}", command, arg_str)
}

