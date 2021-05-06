#![feature(proc_macro_hygiene, decl_macro)]

pub mod contract_vm;

extern crate base64;
extern crate clap;

use crate::contract_vm::analyzer::{Member, INDENT};
use crate::contract_vm::editor::TerminalEditor;
use crate::contract_vm::engine::ContractInstance;
use crate::contract_vm::mock::MockStorage;
use crate::contract_vm::querier::WasmHandler;
use clap::{App, Arg};
use colored::*;
use cosmwasm_std::{Binary, QuerierResult, SystemError, SystemResult, WasmQuery};
use itertools::sorted;
use rocket::fairing::{Fairing, Info, Kind};
use rocket::http::Header;
use rocket::response::content;
use rocket::{Request, Response};
use std::collections::HashMap;
use std::io::{Error, ErrorKind};
use std::path::Path;
use std::{fs, sync, thread, time};

// default const is 'static lifetime
const SENDER_ADDR: &str = "fake_sender_addr";
const DEFAULT_REPLICATED_LIMIT: usize = 1024;

#[macro_use]
extern crate lazy_mut;
lazy_mut! {
    static mut EDITOR: TerminalEditor = TerminalEditor::new();
    static mut ENGINES : HashMap<String, ContractInstance> = HashMap::new();
}

#[macro_use]
extern crate rocket;

fn call_engine(contract_addr: &str, func_type: &str, msg: &str) -> Result<String, String> {
    unsafe {
        match ENGINES.get_mut(contract_addr) {
            None => Err(format!("No such contract: {}", contract_addr)),
            Some(engine) => match base64::decode(msg.as_bytes()) {
                Ok(input) => match String::from_utf8(input) {
                    Ok(param) => Ok(engine.call(func_type, param.as_str()).to_owned()),
                    Err(err) => Err(err.to_string()),
                },
                Err(err) => Err(err.to_string()),
            },
        }
    }
}

#[get("/contract/<address>/init/<msg>")]
fn init_contract(address: String, msg: String) -> content::Json<String> {
    match call_engine(address.as_str(), "init", msg.as_str()) {
        Ok(data) => content::Json(format!(r#"{{"data": {}}}"#, data)),
        Err(err) => content::Json(format!(r#"{{"error": "{}"}}"#, err)),
    }
}

#[get("/contract/<address>/handle/<msg>")]
fn handle_contract(address: String, msg: String) -> content::Json<String> {
    match call_engine(address.as_str(), "handle", msg.as_str()) {
        Ok(data) => content::Json(format!(r#"{{"data": {}}}"#, data)),
        Err(err) => content::Json(format!(r#"{{"error": "{}"}}"#, err)),
    }
}

#[get("/contract/<address>/query/<msg>")]
fn query_contract(address: String, msg: String) -> content::Json<String> {
    match call_engine(address.as_str(), "query", msg.as_str()) {
        Ok(data) => content::Json(format!(r#"{{"data": {}}}"#, data)),
        Err(err) => content::Json(format!(r#"{{"error": "{}"}}"#, err)),
    }
}

// empty struct
pub struct CORS;

impl Fairing for CORS {
    fn info(&self) -> Info {
        Info {
            name: "Add CORS headers to requests",
            kind: Kind::Response,
        }
    }

    fn on_response(&self, _request: &Request, response: &mut Response) {
        response.set_header(Header::new("Access-Control-Allow-Origin", "*"));
        response.set_header(Header::new(
            "Access-Control-Allow-Methods",
            "POST, GET, OPTIONS",
        ));
        response.set_header(Header::new("Access-Control-Allow-Headers", "*"));
        response.set_header(Header::new("Access-Control-Allow-Credentials", "true"));
    }
}

fn start_server(port: u16) {
    // launch server
    thread::spawn(move || {
        let mut config = rocket::config::Config::new(rocket::config::Environment::Production);
        config.set_port(port);
        // prevent multi thread access for easier sharing each cpu
        config.set_workers(1);
        config.set_log_level(rocket::logger::LoggingLevel::Off);

        // launch Restful
        rocket::custom(config)
            .attach(CORS)
            .mount(
                "/wasm",
                routes![init_contract, handle_contract, query_contract],
            )
            .launch()
    });
}

fn query_wasm(request: &WasmQuery) -> QuerierResult {
    unsafe {
        match request {
            WasmQuery::Smart { contract_addr, msg } => {
                match ENGINES.get_mut(contract_addr.as_str()) {
                    None => SystemResult::Err(SystemError::NoSuchContract {
                        addr: contract_addr.to_owned(),
                    }),
                    Some(engine) => {
                        let result = cosmwasm_vm::call_query(
                            &mut engine.instance,
                            &engine.env,
                            msg.as_slice(),
                        );

                        // response can not unwrap, so it is empty
                        match result {
                            Ok(response) => SystemResult::Ok(response),
                            Err(err) => SystemResult::Err(SystemError::InvalidResponse {
                                error: err.to_string(),
                                response: Binary::from([]),
                            }),
                        }
                    }
                }
            }
            _ => SystemResult::Err(SystemError::UnsupportedRequest {
                kind: "Not implemented".to_string(),
            }),
        }
    }
}

fn input_with_out_handle(input_data: &mut String, store_input: bool) -> bool {
    unsafe { EDITOR.readline(input_data, store_input) }
}

fn check_is_need_slash(name: &str) -> bool {
    // Binary is base64 string input
    if name.eq("string") {
        return true;
    }
    return false;
}

fn to_json_item(name: &String, type_name: &str, engine: &ContractInstance) -> String {
    let (strip_type_name, optional) = match type_name.strip_suffix('?') {
        Some(s) => (s, true),
        None => (type_name, false),
    };
    let mut data: String = String::new();
    input_with_out_handle(&mut data, true);

    // do not append optional when empty
    if data.is_empty() && optional {
        return String::new();
    }

    let mapped_type_name = match engine.analyzer.map_of_basetype.get(strip_type_name) {
        None => strip_type_name,
        Some(v) => v,
    };
    let mut params = "\"".to_string();
    params.push_str(name);
    params.push_str("\":");
    if check_is_need_slash(mapped_type_name) {
        params.push('"');
        // clear enter and add slash to double quote
        params.push_str(data.replace('\n', "").replace('"', "\\\"").as_str());
        params.push('"');
    } else {
        params.push_str(data.as_str());
    }

    params.push(',');
    return params;
}

fn input_type(mem_name: &String, type_name: &String, engine: &ContractInstance) -> String {
    println!("input [{}]:", mem_name.blue().bold());
    let st = match engine.analyzer.map_of_struct.get_key_value(type_name) {
        Some(h) => h,
        _ => {
            // return to function, not return to st
            return to_json_item(&mem_name, &type_name, engine);
        }
    };
    //todo:need show all members by recursive invocation
    let mut params = "\"".to_string();
    params.push_str(mem_name);
    params.push_str("\":");

    if st.1.len() == 0 {
        input_with_out_handle(&mut params, true);
    } else {
        params.push('{');
        // member is default sorted
        for members in st.1 {
            println!(
                "input {}[{} : {}]:",
                INDENT,
                members.0.blue().bold(),
                members.1.yellow()
            );
            params.push_str(to_json_item(&members.0, members.1, engine).as_str());
        }
        // remove last , character
        if st.1.len() > 0 {
            params.pop();
        }
        params.push('}');
    }

    params.push(',');

    return params;
}

fn input_message(
    name: &str,
    members: &Vec<Member>,
    engine: &ContractInstance,
    is_enum: &bool,
) -> String {
    let mut final_msg: String = "{".to_string();

    if *is_enum {
        final_msg.push('"');
        final_msg.push_str(name);
        final_msg.push_str("\":{");
    }

    for vcm in members {
        final_msg
            .push_str(input_type(&vcm.member_name, &vcm.member_def.to_string(), engine).as_str());
    }
    if members.len() > 0 {
        final_msg.pop();
    }
    final_msg.push('}');

    if *is_enum {
        final_msg.push('}');
    }

    final_msg
}

fn get_call_type() -> Option<(String, bool)> {
    let mut call_type = String::new();
    let mut params = vec![
        "init".to_string(),
        "handle".to_string(),
        "query".to_string(),
    ];
    let mut need_switch = false;

    print!(
        "Input call type ({} | {} | {}",
        "init".green().bold(),
        "handle".green().bold(),
        "query".green().bold(),
    );
    unsafe {
        if ENGINES.len() > 1 {
            need_switch = true;
        }
        if need_switch {
            print!(" | {}", "switch".blue().bold());
            params.push("switch".to_string());
        }
        EDITOR.update_history_entries(params);
    }
    println!(")");
    input_with_out_handle(&mut call_type, false);
    if call_type.ne("init")
        && call_type.ne("handle")
        && call_type.ne("query")
        && need_switch
        && call_type.ne("switch")
    {
        print!(
            "Wrong call type [{}], must one of ({} | {} | {}",
            call_type.red().bold(),
            "init".green().bold(),
            "handle".green().bold(),
            "query".green().bold(),
        );
        if need_switch {
            print!(" | {}", "switch".green().bold());
        }
        println!(")");
        return None;
    }

    // default messages
    if need_switch && call_type.eq("switch") {
        let mut first = true;
        let mut call_param = String::new();
        print!("Choose smart contract [ ");
        unsafe {
            EDITOR.clear_history();

            for k in sorted(ENGINES.keys()) {
                if first {
                    first = false;
                } else {
                    print!(" | ")
                }
                print!("{}", k.green().bold());
                EDITOR.add_history_entry(k);
            }
        }
        print!(" ]\n");

        input_with_out_handle(&mut call_param, false);

        // check contract existed
        unsafe {
            if ENGINES.get(&call_param).is_none() {
                println!("Smart contract {} not existed", call_param.red().bold());
                return None;
            }
        }

        // return contract as switch param
        return Some((call_param, true));
    }

    return Some((call_type, false));
}

fn simulate_by_auto_analyze(
    engine: &mut ContractInstance,
    sender_addr: &str,
) -> Result<(bool, String), String> {
    // enable debug, show info
    if cfg!(debug_assertions) {
        engine.analyzer.dump_all_members();
        engine.analyzer.dump_all_definitions();
    }

    loop {
        println!(
            "Start_simulate with sender: {}, contract: {}",
            sender_addr.green().bold(),
            engine.env.contract.address.green().bold()
        );

        let (call_type, is_switch) = match get_call_type() {
            None => continue,
            Some(s) => s,
        };

        let mut call_param = String::new();
        let mut first = true;
        // default messages
        if is_switch {
            if engine.env.contract.address.to_string().ne(&call_param) {
                return Ok((true, call_type));
            }
            continue;
        } else if call_type.eq("init") && engine.analyzer.map_of_member.contains_key("InitMsg") {
            call_param = "InitMsg".to_string();
        } else if call_type.eq("handle") && engine.analyzer.map_of_member.contains_key("HandleMsg")
        {
            call_param = "HandleMsg".to_string();
        } else if call_type.eq("query") && engine.analyzer.map_of_member.contains_key("QueryMsg") {
            call_param = "QueryMsg".to_string();
        } else {
            print!("Input Call param from [ ");

            unsafe {
                EDITOR.clear_history();

                for k in sorted(engine.analyzer.map_of_member.keys()) {
                    if first {
                        first = false;
                    } else {
                        print!(" | ")
                    }
                    print!("{}", k.green().bold());
                    EDITOR.add_history_entry(k);
                }
            }

            print!(" ]\n");

            input_with_out_handle(&mut call_param, false);
        }

        // if there is anyOf, it is enum
        let is_enum = engine
            .analyzer
            .map_of_enum
            .get(&call_param)
            .unwrap_or(&false);

        let msg_type: &HashMap<String, Vec<Member>> =
            match engine.analyzer.map_of_member.get(call_param.as_str()) {
                None => {
                    println!("can not find msg type {}", call_param.as_str());
                    continue;
                }
                Some(v) => v,
            };
        let len = msg_type.len();
        if len > 0 {
            //only one msg
            if msg_type.len() == 1 {
                call_param = msg_type.keys().next().unwrap().to_string();
            } else {
                print!("Input Call param from [ ");
                first = true;

                unsafe {
                    EDITOR.clear_history();
                    for k in sorted(msg_type.keys()) {
                        if first {
                            first = false;
                        } else {
                            print!(" | ")
                        }
                        print!("{}", k.green().bold());
                        EDITOR.add_history_entry(k);
                    }
                }

                print!(" ]\n");
                call_param.clear();
                input_with_out_handle(&mut call_param, false);
            }
        }

        let msg = match msg_type.get(call_param.as_str()) {
            None => {
                println!("can not find msg type {}", call_param.as_str());
                continue;
            }
            Some(v) => v,
        };

        engine.analyzer.show_message_type(call_param.as_str(), msg);

        // update previous history entries
        unsafe {
            EDITOR.update_input_history_entry();
        }

        let json_msg = input_message(call_param.as_str(), msg, engine, &is_enum);

        engine.call(call_type.as_str(), json_msg.as_str());
    }
}

fn simulate_by_json(
    engine: &mut ContractInstance,
    sender_addr: &str,
) -> Result<(bool, String), String> {
    loop {
        println!(
            "Start_simulate with sender: {}, contract: {}",
            sender_addr.green().bold(),
            engine.env.contract.address.green().bold()
        );
        let (call_type, is_switch) = match get_call_type() {
            None => continue,
            Some(s) => s,
        };

        // default messages
        if is_switch {
            if engine.env.contract.address.to_string().ne(&call_type) {
                return Ok((true, call_type));
            }
            continue;
        }

        println!("Input json string:");
        let mut json_msg = String::new();
        // update previous history entries
        unsafe {
            EDITOR.update_input_history_entry();
        }

        input_with_out_handle(&mut json_msg, true);

        engine.call(call_type.as_str(), json_msg.as_str());
    }
}

fn start_simulate(contract_addr: &str, sender_addr: &str) -> Result<(bool, String), String> {
    unsafe {
        match ENGINES.get_mut(contract_addr) {
            Some(engine) => {
                // enable debug
                if cfg!(debug_assertions) {
                    engine.show_module_info();
                }
                if engine.analyzer.map_of_member.is_empty() {
                    simulate_by_json(engine, sender_addr)
                } else {
                    simulate_by_auto_analyze(engine, sender_addr)
                }
            }
            None => Err(format!("No engine found: {}", contract_addr)),
        }
    }
}

fn start_simulate_forever(contract_addr: &str, sender_addr: &str) -> bool {
    match start_simulate(contract_addr, sender_addr) {
        Ok(ret) => {
            if ret.0 {
                // recursive
                start_simulate_forever(ret.1.as_str(), sender_addr)
            } else {
                println!(
                    "start_simulate failed for contract: {}",
                    contract_addr.blue().bold()
                );
                return false;
            }
        }
        Err(e) => {
            println!("error occurred during call start_simulate : {}", e.red());
            return false;
        }
    }
}

fn load_artifacts(
    file_path: &str,
    contract_folder: Option<&str>,
) -> Result<Vec<(String, String)>, Error> {
    // check file
    if !file_path.ends_with(".wasm") {
        println!(
            "only support file[*.wasm], you just input a wrong file format - {}",
            file_path.green().bold()
        );
        return Err(Error::new(ErrorKind::InvalidInput, file_path));
    }
    let wasm_file = Path::new(file_path);
    if !wasm_file.is_file() {
        return Err(Error::new(ErrorKind::NotFound, file_path));
    }

    let contract_addr = wasm_file.file_stem().unwrap().to_str().unwrap();
    let mut file_paths = vec![(file_path.to_string(), contract_addr.to_string())];

    let seg = match file_path.rfind('/') {
        None => return Ok(file_paths),
        Some(idx) => idx,
    };
    let (parent_path, _) = file_path.split_at(seg);

    if let Some(folder) = contract_folder {
        let artifacts_folder = std::path::Path::new(parent_path).join(folder);

        if artifacts_folder.is_dir() {
            for entry in std::fs::read_dir(artifacts_folder)? {
                let dir = entry?;
                if let Some(contract_addr) = dir.file_name().to_str() {
                    if let Some(file) = dir.path().to_str() {
                        let wasm_file = Path::new(file).join(format!("{}.wasm", contract_addr));
                        if wasm_file.is_file() {
                            file_paths.push((
                                wasm_file.to_str().unwrap().to_string(),
                                contract_addr.to_string(),
                            ));
                        }
                    }
                }
            }
        }
    }

    Ok(file_paths)
}

fn insert_engine(
    wasm_file: &str,
    contract_addr: &str,
    sender_addr: &str,
    wasm_handler: WasmHandler,
    storage: &MockStorage,
) {
    match ContractInstance::new_instance(
        wasm_file,
        contract_addr,
        sender_addr,
        wasm_handler,
        storage,
    ) {
        Err(e) => {
            println!("error occurred during install contract: {}", e.red());
        }
        Ok(engine) => {
            unsafe { ENGINES.insert(contract_addr.to_owned(), engine) };
        }
    };
}

fn watch_and_update(
    sender: &sync::mpsc::Sender<String>,
    sender_addr: &str,
    wasm_files: &Vec<(String, String)>,
) -> Result<bool, Error> {
    // do not copy, use reference when loop
    let len = wasm_files.len();
    let mut modified_files: Vec<time::SystemTime> = vec![time::SystemTime::now(); len];

    loop {
        for index in 0..len {
            let (wasm_file, contract_addr) = &wasm_files[index];

            if let Ok(modified_time) = fs::metadata(wasm_file)?.modified() {
                if modified_time.eq(&modified_files[index]) {
                    continue;
                }
                modified_files[index] = modified_time;
            }

            unsafe {
                match ENGINES.get_mut(contract_addr) {
                    Some(eng) => {
                        // sleep 100 miliseconds incase it notifies modification before build version is completed
                        thread::sleep(time::Duration::from_millis(100));
                        // callback query directly from storage to copy it
                        eng.instance
                            .with_storage(|storage| {
                                insert_engine(
                                    wasm_file,
                                    contract_addr,
                                    sender_addr,
                                    query_wasm,
                                    storage,
                                );
                                Ok(())
                            })
                            .unwrap();
                    }
                    None => {
                        insert_engine(
                            wasm_file,
                            contract_addr,
                            sender_addr,
                            query_wasm,
                            &contract_vm::mock::MockStorage::default(),
                        );
                    }
                };
            }
        }

        // init all contracts first time, send the first contract to notify
        if len > 0 {
            sender.send(wasm_files[0].1.to_owned()).unwrap();
        }

        // watch again every second
        thread::sleep(time::Duration::from_millis(1000));
    }
}

fn prepare_command_line() -> bool {
    let matches = App::new("cosmwasm-simulate")
        .version("0.1.0")
        .author("github : https://github.com/oraichain/cosmwasm-simulate.git")
        .about("A simulation of cosmwasm smart contract system")
        .arg(
            Arg::with_name("run")
                .help("contract file that built by https://github.com/oraichain/smart-studio.git")
                .empty_values(false),
        )
        .arg(Arg::from_usage(
            "-c, --contract=[CONTRACT_FOLDER] 'Other contract folder'",
        ))
        .arg(Arg::with_name("port").help("port of restful server"))
        .get_matches();

    if let Some(port_str) = matches.value_of("port") {
        if let Ok(port) = port_str.parse() {
            let debug = match std::env::var("DEBUG") {
                Ok(s) => s.ne("false"),
                _ => false,
            };

            if debug {
                println!("🚀 Rest API Base URL: http://0.0.0.0:{}/wasm", port);
            }

            start_server(port);
        }
    }

    if let Some(file) = matches.value_of("run") {
        // start load, check other file as well
        let wasm_files = match load_artifacts(file, matches.value_of("contract")) {
            Err(_) => vec![],
            Ok(s) => s,
        };

        let (sender, receiver) = sync::mpsc::channel();
        // Spawn off an expensive computation
        thread::spawn(move || {
            // trigger init lazy mut
            unsafe {
                ENGINES.init_once();
            }

            if let Ok(ret) = watch_and_update(&sender, SENDER_ADDR, &wasm_files) {
                return ret;
            }
            return true;
        });

        // simulate until break, start with first contract
        match receiver.recv() {
            Ok(contract_addr) => {
                unsafe {
                    // init the first suggested items
                    EDITOR.add_input_history_entry(SENDER_ADDR.to_string());
                    for k in ENGINES.keys() {
                        EDITOR.add_input_history_entry(k.to_owned());
                    }
                }
                return start_simulate_forever(contract_addr.as_str(), SENDER_ADDR);
            }
            Err(e) => {
                println!("watch error: {}", e.to_string().red());
                return false;
            }
        }
    }
    return false;
}

fn main() {
    prepare_command_line();
}
