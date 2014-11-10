use std::os;
use std::io::IoResult;
use std::io::stdio::{stdout, stderr};
use std::rc::Rc;
use std::cell::RefCell;
use std::iter::Peekable;
use std::slice::Items;
use std::hash::Hash;
use std::hash::sip::SipState;
use std::ascii::AsciiExt;
use std::from_str::FromStr;

use std::collections::HashMap;
use std::collections::hash_map::{Vacant, Occupied};
use std::collections::HashSet;

use super::action::Action;
use super::action::{ParseResult, Parsed, Help, Exit, Error};
use super::action::TypedAction;
use super::action::{Flag, Single, Push, Many};
use super::action::IArgAction;
use super::generic::StoreAction;
use super::help::{HelpAction, wrap_text};


static OPTION_WIDTH: uint = 24;
static TOTAL_WIDTH: uint = 79;


enum ArgumentKind {
    Positional,
    ShortOption,
    LongOption,
    Delimiter, // Barely "--"
}

impl ArgumentKind {
    fn check(name: &str) -> ArgumentKind {
        let mut iter = name.chars();
        let char1 = iter.next();
        let char2 = iter.next();
        let char3 = iter.next();
        return match char1 {
            Some('-') => match char2 {
                Some('-') => match char3 {
                        Some(_) => LongOption, // --opt
                        None => Delimiter,  // just --
                },
                Some(_) => ShortOption,  // -opts
                None => Positional,  // single dash
            },
            Some(_) | None => Positional,
        }
    }
}

struct GenericArgument<'parser> {
    id: uint,
    varid: uint,
    name: &'parser str,
    help: &'parser str,
    action: Action<'parser>,
}

struct GenericOption<'parser> {
    id: uint,
    varid: Option<uint>,
    names: Vec<&'parser str>,
    help: &'parser str,
    action: Action<'parser>,
}

struct EnvVar<'parser> {
    varid: uint,
    name: &'parser str,
    action: Box<IArgAction + 'parser>,
}

impl<'a> Hash for GenericOption<'a> {
    fn hash(&self, state: &mut SipState) {
        self.id.hash(state);
    }
}

impl<'a> PartialEq for GenericOption<'a> {
    fn eq(&self, other: &GenericOption<'a>) -> bool {
        return self.id == other.id;
    }
}

impl<'a> Eq for GenericOption<'a> {}

impl<'a> Hash for GenericArgument<'a> {
    fn hash(&self, state: &mut SipState) {
        self.id.hash(state);
    }
}

impl<'a> PartialEq for GenericArgument<'a> {
    fn eq(&self, other: &GenericArgument<'a>) -> bool {
        return self.id == other.id;
    }
}

impl<'a> Eq for GenericArgument<'a> {}

pub struct Var<'parser> {
    id: uint,
    metavar: String,
    required: bool,
}

impl<'parser> Hash for Var<'parser> {
    fn hash(&self, state: &mut SipState) {
        self.id.hash(state);
    }
}

impl<'parser> PartialEq for Var<'parser> {
    fn eq(&self, other: &Var<'parser>) -> bool {
        return self.id == other.id;
    }
}

impl<'a> Eq for Var<'a> {}

struct Context<'ctx, 'parser: 'ctx> {
    parser: &'ctx ArgumentParser<'parser>,
    set_vars: HashSet<uint>,
    list_options: HashMap<Rc<GenericOption<'parser>>, Vec<&'ctx str>>,
    list_arguments: HashMap<Rc<GenericArgument<'parser>>, Vec<&'ctx str>>,
    arguments: Vec<&'ctx str>,
    iter: Peekable<&'ctx String, Items<'ctx, String>>,
    stderr: &'ctx mut Writer + 'ctx,
}

impl<'a, 'b> Context<'a, 'b> {

    fn parse_option(&mut self, opt: Rc<GenericOption<'b>>,
        optarg: Option<&'a str>)
        -> ParseResult
    {
        let value = match optarg {
            Some(value) => value,
            None => match self.iter.next() {
                Some(value) => {
                    let argborrow: &'a str = value.as_slice();
                    argborrow
                }
                None => {
                    return match opt.action {
                        Many(_) => Parsed,
                        _ => Error(format!(
                            "Option {} requires an argument", opt.names)),
                    };
                }
            },
        };
        match opt.varid {
            Some(varid) => { self.set_vars.insert(varid); }
            None => {}
        }
        match opt.action {
            Single(ref action) => {
                return action.parse_arg(value);
            }
            Push(_) => {
                (match self.list_options.entry(opt.clone()) {
                    Occupied(occ) => occ.into_mut(),
                    Vacant(vac) => vac.set(Vec::new()),
                }).push(value);
                return Parsed;
            }
            Many(_) => {
                let vec = match self.list_options.entry(opt.clone()) {
                    Occupied(occ) => occ.into_mut(),
                    Vacant(vac) => vac.set(Vec::new()),
                };
                vec.push(value);
                match optarg {
                    Some(_) => return Parsed,
                    _ => {}
                }
                loop {
                    match self.iter.peek() {
                        None => { break; }
                        Some(arg) if arg.as_slice().starts_with("-") => {
                            break;
                        }
                        Some(value) => {
                            let argborrow: &'a str = value.as_slice();
                            vec.push(argborrow);
                        }
                    }
                    self.iter.next();
                }
                return Parsed;
            }
            _ => panic!(),
        };
    }

    fn parse_long_option(&mut self, arg: &'a str) -> ParseResult {
        let mut equals_iter = arg.splitn(1, '=');
        let optname = equals_iter.next().unwrap();
        let valueref = equals_iter.next();
        let opt = self.parser.long_options.get(&optname.to_string());
        match opt {
            Some(opt) => {
                match opt.action {
                    Flag(ref action) => {
                        match valueref {
                            Some(_) => {
                                return Error(format!(
                                    "Option {} does not accept an argument",
                                    optname));
                            }
                            None => {
                                match opt.varid {
                                    Some(varid) => {
                                        self.set_vars.insert(varid);
                                    }
                                    None => {}
                                }
                                return action.parse_flag();
                            }
                        }
                    }
                    Single(_) | Push(_) | Many(_) => {
                        return self.parse_option(opt.clone(), valueref);
                    }
                }
            }
            None => {
                return Error(format!("Unknown option {}", arg));
            }
        }
    }

    fn parse_short_options<'x>(&'x mut self, arg: &'a str) -> ParseResult {
        let mut iter = arg.char_indices();
        iter.next();
        for (idx, ch) in iter {
            let opt = match self.parser.short_options.get(&ch) {
                Some(opt) => { opt }
                None => {
                    return Error(format!("Unknown short option \"{}\"", ch));
                }
            };
            let res = match opt.action {
                Flag(ref action) => {
                    match opt.varid {
                        Some(varid) => { self.set_vars.insert(varid); }
                        None => {}
                    }
                    action.parse_flag()
                }
                Single(_) | Push(_) | Many(_) => {
                    let value;
                    if idx + 1 < arg.len() {
                        value = Some(arg.slice(idx+1, arg.len()));
                    } else {
                        value = None;
                    }
                    return self.parse_option(opt.clone(), value);
                }
            };
            match res {
                Parsed => { continue; }
                x => { return x; }
            }
        }
        return Parsed;
    }

    fn postpone_argument(&mut self, arg: &'a str) {
        self.arguments.push(arg);
    }

    fn parse_options(&mut self) -> ParseResult {
        self.iter.next();  // Command name
        loop {
            let next = self.iter.next();
            let arg = match next {
                Some(arg) => { arg }
                None => { break; }
            };
            let res = match ArgumentKind::check(arg.as_slice()) {
                Positional => {
                    self.postpone_argument(arg.as_slice());
                    if self.parser.stop_on_first_argument {
                        break;
                    }
                    continue;
                }
                LongOption => self.parse_long_option(arg.as_slice()),
                ShortOption => self.parse_short_options(arg.as_slice()),
                Delimiter => break,
            };
            match res {
                Parsed => continue,
                _ => return res,
            }
        }

        loop {
            match self.iter.next() {
                None => break,
                Some(arg) => self.postpone_argument(arg.as_slice()),
            }
        }
        return Parsed;
    }

    fn parse_arguments(&mut self) -> ParseResult {
        let mut pargs = self.parser.arguments.iter();
        for arg in self.arguments.iter() {
            let mut opt;
            loop {
                match pargs.next() {
                    Some(option) => {
                        if self.set_vars.contains(&option.varid) {
                            continue;
                        }
                        opt = option;
                        break;
                    }
                    None => match self.parser.catchall_argument {
                        Some(ref option) => {
                            opt = option;
                            break;
                        }
                        None => return Error(format!(
                            "Unexpected argument {}", arg)),
                    }
                };
            }
            let res = match opt.action {
                Single(ref act) => {
                    self.set_vars.insert(opt.varid);
                    act.parse_arg(*arg)
                },
                Many(_) | Push(_) => {
                    (match self.list_arguments.entry(opt.clone()) {
                        Occupied(occ) => occ.into_mut(),
                        Vacant(vac) => vac.set(Vec::new()),
                    }).push(*arg);
                    Parsed
                },
                _ => unreachable!(),
            };
            match res {
                Parsed => continue,
                _ => return res,
            }
        }
        return Parsed;
    }

    fn parse_list_vars(&mut self) -> ParseResult {
        for (opt, lst) in self.list_options.iter() {
            match opt.action {
                Push(ref act) | Many(ref act) => {
                    let res = act.parse_args(lst.as_slice());
                    match res {
                        Parsed => continue,
                        _ => return res,
                    }
                }
                _ => panic!(),
            }
        }
        for (opt, lst) in self.list_arguments.iter() {
            match opt.action {
                Push(ref act) | Many(ref act) => {
                    let res = act.parse_args(lst.as_slice());
                    match res {
                        Parsed => continue,
                        _ => return res,
                    }
                }
                _ => panic!(),
            }
        }
        return Parsed;
    }

    fn parse_env_vars(&mut self) -> ParseResult {
        for evar in self.parser.env_vars.iter() {
            match os::getenv(evar.name) {
                Some(val) => {
                    match evar.action.parse_arg(val.as_slice()) {
                        Parsed => {
                            self.set_vars.insert(evar.varid);
                            continue;
                        }
                        Error(err) => {
                            self.stderr.write_str(format!(
                                "WARNING: Environment variable {}: {}\n",
                                evar.name, err).as_slice()).ok();
                        }
                        _ => unreachable!(),
                    }
                }
                None => {}
            }
        }
        return Parsed;
    }

    fn check_required(&mut self) -> ParseResult {
        // Check for required arguments
        for var in self.parser.vars.iter() {
            if var.required && !self.set_vars.contains(&var.id) {
                // First try positional arguments
                for opt in self.parser.arguments.iter() {
                    if opt.varid == var.id {
                        return Error(format!(
                            "Argument {} is required", opt.name));
                    }
                }
                // Then options
                for opt in self.parser.options.iter() {
                    match opt.varid {
                        Some(varid) if varid == var.id => {}
                        _ => { continue }
                    }
                    return Error(format!(
                        "Option {} is required", opt.names));
                }
                // Then envvars
                for envvar in self.parser.env_vars.iter() {
                    if envvar.varid == var.id {
                        return Error(format!(
                            "Environment var {} is required", envvar.name));
                    }
                }
            }
        }
        return Parsed;
    }

    fn parse(parser: &ArgumentParser, args: &Vec<String>, stderr: &mut Writer)
        -> ParseResult
    {
        let mut ctx = Context {
            parser: parser,
            iter: args.iter().peekable(),
            set_vars: HashSet::new(),
            list_options: HashMap::new(),
            list_arguments: HashMap::new(),
            arguments: Vec::new(),
            stderr: stderr,
        };

        match ctx.parse_env_vars() {
            Parsed => {}
            x => { return x; }
        }

        match ctx.parse_options() {
            Parsed => {}
            x => { return x; }
        }

        match ctx.parse_arguments() {
            Parsed => {}
            x => { return x; }
        }

        match ctx.parse_list_vars() {
            Parsed => {}
            x => { return x; }
        }

        match ctx.check_required() {
            Parsed => {}
            x => { return x; }
        }

        return Parsed;
    }
}

pub struct Ref<'parser, T: 'parser> {
    cell: Rc<RefCell<&'parser mut T>>,
    varid: uint,
    parser: &'parser mut ArgumentParser<'parser>,
}

impl<'parser, T> Ref<'parser, T> {

    pub fn add_option<'x>(&'x mut self, names: &[&'parser str],
        action: Box<TypedAction<T>>, help: &'parser str)
        -> &'x mut Ref<'parser, T>
    {
        {
            let var = &mut self.parser.vars.as_mut_slice()[self.varid];
            if var.metavar.len() == 0 {
                let mut longest_name = names[0];
                let mut llen = longest_name.len();
                for name in names.iter() {
                    if name.len() > llen {
                        longest_name = *name;
                        llen = longest_name.len();
                    }
                }
                if llen > 2 {
                    var.metavar = longest_name.slice(2, llen)
                        .to_ascii_upper().replace("-", "_");
                }
            }
        }
        self.parser.add_option_for(Some(self.varid), names,
            action.bind(self.cell.clone()),
            help);
        return self;
    }

    pub fn add_argument<'x>(&'x mut self, name: &'parser str,
        action: Box<TypedAction<T>>, help: &'parser str)
        -> &'x mut Ref<'parser, T>
    {
        let act = action.bind(self.cell.clone());
        let opt = Rc::new(GenericArgument {
            id: self.parser.arguments.len(),
            varid: self.varid,
            name: name,
            help: help,
            action: act,
            });
        match opt.action {
            Flag(_) => panic!("Flag arguments can't be positional"),
            Many(_) | Push(_) => {
                match self.parser.catchall_argument {
                    Some(ref y) => panic!(format!(
                        "Option {} conflicts with option {}",
                        name, y.name)),
                    None => {},
                }
                self.parser.catchall_argument = Some(opt);
            }
            Single(_) => {
                self.parser.arguments.push(opt);
            }
        }
        {
            let var = &mut self.parser.vars.as_mut_slice()[self.varid];
            if var.metavar.len() == 0 {
                var.metavar = name.to_string();
            }
        }
        return self;
    }

    pub fn metavar<'x>(&'x mut self, name: &str)
        -> &'x mut Ref<'parser, T>
    {
        {
            let var = &mut self.parser.vars.as_mut_slice()[self.varid];
            var.metavar = name.to_string();
        }
        return self;
    }

    pub fn required<'x>(&'x mut self)
        -> &'x mut Ref<'parser, T>
    {
        {
            let var = &mut self.parser.vars.as_mut_slice()[self.varid];
            var.required = true;
        }
        return self;
    }
}

impl<'parser, T: 'static + FromStr> Ref<'parser, T> {
    pub fn envvar<'x>(&'x mut self, varname: &'parser str)
        -> &'x mut Ref<'parser, T>
    {
        self.parser.env_vars.push(Rc::new(EnvVar {
            varid: self.varid,
            name: varname,
            action: box StoreAction { cell: self.cell.clone() },
            }));
        return self;
    }
}

pub struct ArgumentParser<'a> {
    description: &'a str,
    vars: Vec<Box<Var<'a>>>,
    options: Vec<Rc<GenericOption<'a>>>,
    arguments: Vec<Rc<GenericArgument<'a>>>,
    env_vars: Vec<Rc<EnvVar<'a>>>,
    catchall_argument: Option<Rc<GenericArgument<'a>>>,
    short_options: HashMap<char, Rc<GenericOption<'a>>>,
    long_options: HashMap<String, Rc<GenericOption<'a>>>,
    stop_on_first_argument: bool,
}



impl<'parser> ArgumentParser<'parser> {

    pub fn new() -> ArgumentParser<'parser> {

        let mut ap = ArgumentParser {
            description: "",
            vars: Vec::new(),
            env_vars: Vec::new(),
            arguments: Vec::new(),
            catchall_argument: None,
            options: Vec::new(),
            short_options: HashMap::new(),
            long_options: HashMap::new(),
            stop_on_first_argument: false,
            };
        ap.add_option_for(None, ["-h", "--help"], Flag(box HelpAction),
            "show this help message and exit");
        return ap;
    }

    pub fn refer<T>(&'parser mut self, val: &'parser mut T)
        -> Box<Ref<'parser, T>>
    {
        let cell = Rc::new(RefCell::new(val));
        let id = self.vars.len();
        self.vars.push(box Var {
                id: id,
                required: false,
                metavar: "".to_string(),
                });
        return box Ref {
            cell: cell.clone(),
            varid: id,
            parser: self,
        };
    }

    pub fn set_description(&mut self, descr: &'parser str) {
        self.description = descr;
    }

    fn add_option_for(&mut self, var: Option<uint>,
        names: &[&'parser str],
        action: Action<'parser>, help: &'parser str)
    {
        let opt = Rc::new(GenericOption {
            id: self.options.len(),
            varid: var,
            names: names.to_vec(),
            help: help,
            action: action,
            });

        if names.len() < 1 {
            panic!("At least one name for option must be specified");
        }
        for nameptr in names.iter() {
            let name = *nameptr;
            match ArgumentKind::check(name) {
                Positional|Delimiter => {
                    panic!("Bad argument name {}", name);
                }
                LongOption => {
                    self.long_options.insert(
                        name.to_string(), opt.clone());
                }
                ShortOption => {
                    if name.len() > 2 {
                        panic!("Bad short argument {}", name);
                    }
                    self.short_options.insert(
                        name.as_bytes()[1] as char, opt.clone());
                }
            }
        }
        self.options.push(opt);
    }

    pub fn print_help(&self, name: &str, writer: &mut Writer) -> IoResult<()> {
        return HelpFormatter::print_help(self, name, writer);
    }

    pub fn print_usage(&self, name: &str, writer: &mut Writer) -> IoResult<()>
    {
        return HelpFormatter::print_usage(self, name, writer);
    }

    pub fn parse(&self, args: Vec<String>,
        stdout: &mut Writer, stderr: &mut Writer)
        -> Result<(), int>
    {
        match Context::parse(self, &args, stderr) {
            Parsed => return Ok(()),
            Exit => return Err(0),
            Help => {
                self.print_help(args[0].as_slice(), stdout).unwrap();
                return Err(0);
            }
            Error(message) => {
                self.error(args[0].as_slice(), message.as_slice(), stderr);
                return Err(2);
            }
        }
    }

    pub fn error(&self, command: &str, message: &str, writer: &mut Writer) {
        self.print_usage(command, writer).unwrap();
        writer.write_str(
            format!("{}: {}\n", command, message).as_slice()
        ).unwrap();
    }

    pub fn stop_on_first_argument(&mut self, want_stop: bool) {
        self.stop_on_first_argument = want_stop;
    }

    pub fn parse_args(&self) -> Result<(), int> {
        return self.parse(os::args(), &mut stdout(), &mut stderr());
    }
}

struct HelpFormatter<'a, 'b: 'a> {
    name: &'a str,
    parser: &'a ArgumentParser<'b>,
    buf: &'a mut Writer + 'a,
}

impl<'a, 'b> HelpFormatter<'a, 'b> {
    pub fn print_usage(parser: &ArgumentParser, name: &str, writer: &mut Writer)
        -> IoResult<()>
    {
        return HelpFormatter { parser: parser, name: name, buf: writer }
            .write_usage();
    }

    pub fn print_help(parser: &ArgumentParser, name: &str, writer: &mut Writer)
        -> IoResult<()>
    {
        return HelpFormatter { parser: parser, name: name, buf: writer }
            .write_help();
    }

    pub fn print_argument(&mut self, arg: &GenericArgument<'b>)
        -> IoResult<()>
    {
        let mut num = 2;
        try!(self.buf.write_str("  "));
        try!(self.buf.write_str(arg.name));
        num += arg.name.len();
        if num >= OPTION_WIDTH {
            try!(self.buf.write_char('\n'));
            for _ in range(0, OPTION_WIDTH) {
                try!(self.buf.write_char(' '));
            }
        } else {
            for _ in range(num, OPTION_WIDTH) {
                try!(self.buf.write_char(' '));
            }
        }
        try!(wrap_text(self.buf, arg.help, TOTAL_WIDTH, OPTION_WIDTH));
        try!(self.buf.write_char('\n'));
        return Ok(());
    }

    pub fn print_option(&mut self, opt: &GenericOption<'b>) -> IoResult<()> {
        let mut num = 2;
        try!(self.buf.write_str("  "));
        let mut niter = opt.names.iter();
        let name = niter.next().unwrap();
        try!(self.buf.write_str(*name));
        num += name.len();
        for name in niter {
            try!(self.buf.write_char(','));
            try!(self.buf.write_str(*name));
            num += name.len() + 1;
        }
        match opt.action {
            Flag(_) => {}
            Single(_) | Push(_) | Many(_) => {
                try!(self.buf.write_char(' '));
                let var = &self.parser.vars.as_slice()[opt.varid.unwrap()];
                try!(self.buf.write_str(var.metavar.as_slice()));
                num += var.metavar.len() + 1;
            }
        }
        if num >= OPTION_WIDTH {
            try!(self.buf.write_char('\n'));
            for _ in range(0, OPTION_WIDTH) {
                try!(self.buf.write_char(' '));
            }
        } else {
            for _ in range(num, OPTION_WIDTH) {
                try!(self.buf.write_char(' '));
            }
        }
        try!(wrap_text(self.buf, opt.help, TOTAL_WIDTH, OPTION_WIDTH));
        try!(self.buf.write_char('\n'));
        return Ok(());
    }

    fn write_help(&mut self) -> IoResult<()> {
        try!(self.write_usage());
        try!(self.buf.write_char('\n'));
        if self.parser.description.len() > 0 {
            try!(wrap_text(self.buf, self.parser.description,TOTAL_WIDTH, 0));
            try!(self.buf.write_char('\n'));
        }
        if self.parser.arguments.len() > 0
            || self.parser.catchall_argument.is_some()
        {
            try!(self.buf.write_str("\npositional arguments:\n"));
            for arg in self.parser.arguments.iter() {
                try!(self.print_argument(&**arg));
            }
            match self.parser.catchall_argument {
                Some(ref opt) => {
                    try!(self.print_argument(&**opt));
                }
                None => {}
            }
        }
        if self.parser.short_options.len() > 0
            || self.parser.long_options.len() > 0
        {
            try!(self.buf.write_str("\noptional arguments:\n"));
            for opt in self.parser.options.iter() {
                try!(self.print_option(&**opt));
            }
        }
        return Ok(());
    }

    fn write_usage(&mut self) -> IoResult<()> {
        try!(self.buf.write_str("Usage:\n    "));
        try!(self.buf.write(self.name.as_bytes()));
        if self.parser.options.len() != 0 {
            if self.parser.short_options.len() > 1
                || self.parser.long_options.len() > 1
            {
                try!(self.buf.write_str(" [OPTIONS]"));
            }
            for opt in self.parser.arguments.iter() {
                let var = &self.parser.vars.as_slice()[opt.varid];
                try!(self.buf.write_char(' '));
                if !var.required {
                    try!(self.buf.write_char('['));
                }
                try!(self.buf.write_str(opt.name.to_ascii_upper().as_slice()));
                if !var.required {
                    try!(self.buf.write_char(']'));
                }
            }
            match self.parser.catchall_argument {
                Some(ref opt) => {
                    let var = &self.parser.vars.as_slice()[opt.varid];
                    try!(self.buf.write_char(' '));
                    if !var.required {
                        try!(self.buf.write_char('['));
                    }
                    try!(self.buf.write_str(
                        opt.name.to_ascii_upper().as_slice()));
                    if !var.required {
                        try!(self.buf.write_str(" ...]"));
                    } else {
                        try!(self.buf.write_str(" [...]"));
                    }
                }
                None => {}
            }
        }
        try!(self.buf.write_char('\n'));
        return Ok(());
    }

}
