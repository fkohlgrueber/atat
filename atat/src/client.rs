use embedded_hal::{serial, timer::CountDown};

use crate::queues::{ComProducer, ResConsumer, UrcConsumer, UrcItem};
use crate::traits::{AtatClient, AtatCmd, AtatUrc};
use crate::{error::Error, queues::ResCapacity};
use crate::{Command, Config};
use heapless::{consts, ArrayLength};
use typenum::Unsigned;

#[derive(Debug, PartialEq)]
enum ClientState {
    Idle,
    AwaitingResponse,
}

/// Whether the AT client should block while waiting responses or return early.
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub enum Mode {
    /// The function call will wait as long as necessary to complete the operation
    Blocking,
    /// The function call will not wait at all to complete the operation, and only do what it can.
    NonBlocking,
    /// The function call will wait only up the max timeout of each command to complete the operation.
    Timeout,
}

/// Client responsible for handling send, receive and timeout from the
/// userfacing side. The client is decoupled from the ingress-manager through
/// some spsc queue consumers, where any received responses can be dequeued. The
/// Client also has an spsc producer, to allow signaling commands like
/// `reset` to the ingress-manager.
pub struct Client<Tx, T, BufLen = consts::U256, UrcCapacity = consts::U10>
where
    Tx: serial::Write<u8>,
    T: CountDown,
    BufLen: ArrayLength<u8>,
    UrcCapacity: ArrayLength<UrcItem<BufLen>>,
{
    /// Serial writer
    tx: Tx,

    /// The response consumer receives responses from the ingress manager
    res_c: ResConsumer<BufLen>,
    /// The URC consumer receives URCs from the ingress manager
    urc_c: UrcConsumer<BufLen, UrcCapacity>,
    /// The command producer can send commands to the ingress manager
    com_p: ComProducer,

    state: ClientState,
    timer: T,
    config: Config,
}

impl<Tx, T, BufLen, UrcCapacity> Client<Tx, T, BufLen, UrcCapacity>
where
    Tx: serial::Write<u8>,
    T: CountDown,
    T::Time: From<u32>,
    BufLen: ArrayLength<u8>,
    UrcCapacity: ArrayLength<UrcItem<BufLen>>,
{
    pub fn new(
        tx: Tx,
        res_c: ResConsumer<BufLen>,
        urc_c: UrcConsumer<BufLen, UrcCapacity>,
        com_p: ComProducer,
        timer: T,
        config: Config,
    ) -> Self {
        Self {
            tx,
            res_c,
            urc_c,
            com_p,
            state: ClientState::Idle,
            config,
            timer,
        }
    }
}

impl<Tx, T, BufLen, UrcCapacity> AtatClient for Client<Tx, T, BufLen, UrcCapacity>
where
    Tx: serial::Write<u8>,
    T: CountDown,
    T::Time: From<u32>,
    BufLen: ArrayLength<u8>,
    UrcCapacity: ArrayLength<UrcItem<BufLen>>,
{
    fn send<A: AtatCmd>(&mut self, cmd: &A) -> nb::Result<A::Response, Error<A::Error>> {
        if let ClientState::Idle = self.state {
            if cmd.force_receive_state() && self.com_p.enqueue(Command::ForceReceiveState).is_err()
            {
                // TODO: Consider how to act in this situation.
                defmt::error!(
                    "Failed to signal parser to force state transition to 'ReceivingResponse'!"
                );
            }

            // compare the time of the last response or URC and ensure at least
            // `self.config.cmd_cooldown` ms have passed before sending a new
            // command
            nb::block!(self.timer.wait()).ok();
            let cmd_buf = cmd.as_bytes();

            if cmd_buf.len() < 50 {
                defmt::debug!("Sending command: \"{=[u8]:a}\"", &cmd_buf);
            } else {
                defmt::debug!(
                    "Sending command with too long payload ({} bytes) to log!",
                    cmd_buf.len()
                );
            }

            for c in cmd_buf {
                nb::block!(self.tx.write(c)).map_err(|_e| Error::Write)?;
            }
            nb::block!(self.tx.flush()).map_err(|_e| Error::Write)?;
            self.state = ClientState::AwaitingResponse;
        }

        if !cmd.expects_response_code() {
            self.state = ClientState::Idle;
            return cmd.parse(Ok(&[])).map_err(nb::Error::Other);
        }

        match self.config.mode {
            Mode::Blocking => Ok(nb::block!(self.check_response(cmd))?),
            Mode::NonBlocking => self.check_response(cmd),
            Mode::Timeout => {
                self.timer.start(cmd.max_timeout_ms()).ok();
                Ok(nb::block!(self.check_response(cmd))?)
            }
        }
    }

    fn peek_urc_with<URC: AtatUrc, F: FnOnce(URC::Response) -> bool>(&mut self, f: F) {
        if let Some(urc) = self.urc_c.peek() {
            self.timer.start(self.config.cmd_cooldown).ok();
            if let Some(urc) = URC::parse(urc) {
                if !f(urc) {
                    return;
                }
            } else {
                defmt::error!("Parsing URC FAILED: {=[u8]:a}", urc)
            }
            unsafe { self.urc_c.dequeue_unchecked() };
        }
    }

    fn check_response<A: AtatCmd>(&mut self, cmd: &A) -> nb::Result<A::Response, Error<A::Error>> {
        if let Some(result) = self.res_c.dequeue() {
            return cmd
                .parse(result.as_deref())
                .map_err(nb::Error::from)
                .and_then(|r| {
                    if let ClientState::AwaitingResponse = self.state {
                        self.timer.start(self.config.cmd_cooldown).ok();
                        self.state = ClientState::Idle;
                        Ok(r)
                    } else {
                        // FIXME: Is this correct?
                        defmt::error!("Is this correct?! WouldBlock");
                        Err(nb::Error::WouldBlock)
                    }
                })
                .map_err(|e| {
                    self.timer.start(self.config.cmd_cooldown).ok();
                    self.state = ClientState::Idle;
                    e
                });
        } else if let Mode::Timeout = self.config.mode {
            if self.timer.wait().is_ok() {
                self.state = ClientState::Idle;
                // Tell the parser to reset to initial state due to timeout
                if self.com_p.enqueue(Command::Reset).is_err() {
                    // TODO: Consider how to act in this situation.
                    defmt::error!("Failed to signal parser to clear buffer on timeout!");
                }
                return Err(nb::Error::Other(Error::Timeout));
            }
        }
        Err(nb::Error::WouldBlock)
    }

    fn get_mode(&self) -> Mode {
        self.config.mode
    }

    fn reset(&mut self) {
        if self.com_p.enqueue(Command::Reset).is_err() {
            // TODO: Consider how to act in this situation.
            defmt::error!("Failed to signal ingress manager to reset!");
        }

        for _ in 0..ResCapacity::USIZE {
            if self.res_c.dequeue().is_none() {
                break;
            }
        }
        for _ in 0..UrcCapacity::USIZE {
            if self.urc_c.dequeue().is_none() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::queues;
    use crate::{self as atat, InternalError};
    use crate::{
        atat_derive::{AtatCmd, AtatEnum, AtatResp, AtatUrc},
        GenericError,
    };
    use heapless::{consts, spsc::Queue, String, Vec};
    use nb;

    struct CdMock;

    impl CountDown for CdMock {
        type Error = core::convert::Infallible;
        type Time = u32;
        fn start<T>(&mut self, _count: T) -> Result<(), Self::Error>
        where
            T: Into<Self::Time>,
        {
            Ok(())
        }
        fn wait(&mut self) -> nb::Result<(), Self::Error> {
            Ok(())
        }
    }

    struct TxMock {
        s: String<consts::U64>,
    }

    impl TxMock {
        fn new(s: String<consts::U64>) -> Self {
            TxMock { s }
        }
    }

    impl serial::Write<u8> for TxMock {
        type Error = ();

        fn try_write(&mut self, c: u8) -> nb::Result<(), Self::Error> {
            self.s.push(c as char).map_err(nb::Error::Other)
        }

        fn try_flush(&mut self) -> nb::Result<(), Self::Error> {
            Ok(())
        }
    }

    #[derive(Debug, defmt::Format, PartialEq, Eq)]
    pub enum InnerError {
        Test,
    }

    impl core::str::FromStr for InnerError {
        // This error will always get mapped to `atat::Error::Parse`
        type Err = ();

        fn from_str(_s: &str) -> Result<Self, Self::Err> {
            Ok(Self::Test)
        }
    }

    #[derive(Debug, PartialEq, AtatCmd)]
    #[at_cmd("+CFUN", NoResponse, error = "InnerError")]
    struct ErrorTester {
        x: u8,
    }

    #[derive(Clone, AtatCmd)]
    #[at_cmd("+CFUN", NoResponse, timeout_ms = 180000)]
    pub struct SetModuleFunctionality {
        #[at_arg(position = 0)]
        pub fun: Functionality,
        #[at_arg(position = 1)]
        pub rst: Option<ResetMode>,
    }

    #[derive(Clone, AtatCmd)]
    #[at_cmd("+FUN", NoResponse, timeout_ms = 180000)]
    pub struct Test2Cmd {
        #[at_arg(position = 1)]
        pub fun: Functionality,
        #[at_arg(position = 0)]
        pub rst: Option<ResetMode>,
    }
    #[derive(Clone, AtatCmd)]
    #[at_cmd("+CUN", TestResponseVec, timeout_ms = 180000)]
    pub struct TestRespVecCmd {
        #[at_arg(position = 0)]
        pub fun: Functionality,
        #[at_arg(position = 1)]
        pub rst: Option<ResetMode>,
    }
    #[derive(Clone, AtatCmd)]
    #[at_cmd("+CUN", TestResponseString, timeout_ms = 180000)]
    pub struct TestRespStringCmd {
        #[at_arg(position = 0)]
        pub fun: Functionality,
        #[at_arg(position = 1)]
        pub rst: Option<ResetMode>,
    }
    #[derive(Clone, AtatCmd)]
    #[at_cmd("+CUN", TestResponseStringMixed, timeout_ms = 180000)]
    pub struct TestRespStringMixCmd {
        #[at_arg(position = 1)]
        pub fun: Functionality,
        #[at_arg(position = 0)]
        pub rst: Option<ResetMode>,
    }

    // #[derive(Clone, AtatCmd)]
    // #[at_cmd("+CUN", TestResponseStringMixed, timeout_ms = 180000)]
    // pub struct TestUnnamedStruct(Functionality, Option<ResetMode>);

    #[derive(Clone, PartialEq, AtatEnum)]
    #[at_enum(u8)]
    pub enum Functionality {
        #[at_arg(value = 0)]
        Min,
        #[at_arg(value = 1)]
        Full,
        #[at_arg(value = 4)]
        APM,
        #[at_arg(value = 6)]
        DM,
    }

    #[derive(Clone, PartialEq, AtatEnum)]
    #[at_enum(u8)]
    pub enum ResetMode {
        #[at_arg(value = 0)]
        DontReset,
        #[at_arg(value = 1)]
        Reset,
    }
    #[derive(Clone, AtatResp, PartialEq, Debug)]
    pub struct NoResponse;
    #[derive(Clone, AtatResp, PartialEq, Debug)]
    pub struct TestResponseVec {
        #[at_arg(position = 0)]
        pub socket: u8,
        #[at_arg(position = 1)]
        pub length: usize,
        #[at_arg(position = 2)]
        pub data: Vec<u8, TestRxBufLen>,
    }

    #[derive(Clone, AtatResp, PartialEq, Debug)]
    pub struct TestResponseString {
        #[at_arg(position = 0)]
        pub socket: u8,
        #[at_arg(position = 1)]
        pub length: usize,
        #[at_arg(position = 2)]
        pub data: String<consts::U64>,
    }

    #[derive(Clone, AtatResp, PartialEq, Debug)]
    pub struct TestResponseStringMixed {
        #[at_arg(position = 1)]
        pub socket: u8,
        #[at_arg(position = 2)]
        pub length: usize,
        #[at_arg(position = 0)]
        pub data: String<consts::U64>,
    }

    #[derive(Clone, AtatResp)]
    pub struct MessageWaitingIndication {
        #[at_arg(position = 0)]
        pub status: u8,
        #[at_arg(position = 1)]
        pub code: u8,
    }

    #[derive(Clone, AtatUrc)]
    pub enum Urc {
        #[at_urc(b"+UMWI")]
        MessageWaitingIndication(MessageWaitingIndication),
    }

    type TestRxBufLen = consts::U256;
    type TestUrcCapacity = consts::U10;

    macro_rules! setup {
        ($config:expr) => {{
            static mut RES_Q: queues::ResQueue<TestRxBufLen> = Queue(heapless::i::Queue::u8());
            let (res_p, res_c) = unsafe { RES_Q.split() };
            static mut URC_Q: queues::UrcQueue<TestRxBufLen, TestUrcCapacity> =
                Queue(heapless::i::Queue::u8());
            let (urc_p, urc_c) = unsafe { URC_Q.split() };
            static mut COM_Q: queues::ComQueue = Queue(heapless::i::Queue::u8());
            let (com_p, _com_c) = unsafe { COM_Q.split() };

            let tx_mock = TxMock::new(String::new());
            let client: Client<TxMock, CdMock, TestRxBufLen, TestUrcCapacity> =
                Client::new(tx_mock, res_c, urc_c, com_p, CdMock, $config);
            (client, res_p, urc_p)
        }};
    }

    #[test]
    fn error_response() {
        let (mut client, mut p, _) = setup!(Config::new(Mode::Blocking));

        let cmd = ErrorTester { x: 7 };

        p.enqueue(Err(InternalError::Error(Vec::new()))).unwrap();

        assert_eq!(client.state, ClientState::Idle);
        assert_eq!(
            nb::block!(client.send(&cmd)),
            Err(Error::Error(InnerError::Test))
        );
        assert_eq!(client.state, ClientState::Idle);
    }

    #[test]
    fn generic_error_response() {
        let (mut client, mut p, _) = setup!(Config::new(Mode::Blocking));

        let cmd = SetModuleFunctionality {
            fun: Functionality::APM,
            rst: Some(ResetMode::DontReset),
        };

        p.enqueue(Err(InternalError::Error(Vec::new()))).unwrap();

        assert_eq!(client.state, ClientState::Idle);
        assert_eq!(
            nb::block!(client.send(&cmd)),
            Err(Error::Error(GenericError))
        );
        assert_eq!(client.state, ClientState::Idle);
    }

    #[test]
    fn string_sent() {
        let (mut client, mut p, _) = setup!(Config::new(Mode::Blocking));

        let cmd = SetModuleFunctionality {
            fun: Functionality::APM,
            rst: Some(ResetMode::DontReset),
        };

        p.enqueue(Ok(Vec::<u8, TestRxBufLen>::new())).unwrap();

        assert_eq!(client.state, ClientState::Idle);
        assert_eq!(client.send(&cmd), Ok(NoResponse));
        assert_eq!(client.state, ClientState::Idle);

        assert_eq!(
            client.tx.s,
            String::<consts::U32>::from("AT+CFUN=4,0\r\n"),
            "Wrong encoding of string"
        );

        p.enqueue(Ok(Vec::<u8, TestRxBufLen>::new())).unwrap();

        let cmd = Test2Cmd {
            fun: Functionality::DM,
            rst: Some(ResetMode::Reset),
        };
        assert_eq!(client.send(&cmd), Ok(NoResponse));

        assert_eq!(
            client.tx.s,
            String::<consts::U32>::from("AT+CFUN=4,0\r\nAT+FUN=1,6\r\n"),
            "Reverse order string did not match"
        );
    }

    #[test]
    #[ignore]
    fn countdown() {
        let (mut client, _, _) = setup!(Config::new(Mode::Timeout));

        assert_eq!(client.state, ClientState::Idle);

        let cmd = Test2Cmd {
            fun: Functionality::DM,
            rst: Some(ResetMode::Reset),
        };
        assert_eq!(client.send(&cmd), Err(nb::Error::Other(Error::Timeout)));

        match client.config.mode {
            Mode::Timeout => {} // assert_eq!(cd_mock.time, 180000),
            _ => panic!("Wrong AT mode"),
        }
        assert_eq!(client.state, ClientState::Idle);
    }

    #[test]
    fn blocking() {
        let (mut client, mut p, _) = setup!(Config::new(Mode::Blocking));

        let cmd = SetModuleFunctionality {
            fun: Functionality::APM,
            rst: Some(ResetMode::DontReset),
        };

        p.enqueue(Ok(Vec::<u8, TestRxBufLen>::new())).unwrap();

        assert_eq!(client.state, ClientState::Idle);
        assert_eq!(client.send(&cmd), Ok(NoResponse));
        assert_eq!(client.state, ClientState::Idle);
        assert_eq!(client.tx.s, String::<consts::U32>::from("AT+CFUN=4,0\r\n"));
    }

    #[test]
    fn non_blocking() {
        let (mut client, mut p, _) = setup!(Config::new(Mode::NonBlocking));

        let cmd = SetModuleFunctionality {
            fun: Functionality::APM,
            rst: Some(ResetMode::DontReset),
        };

        assert_eq!(client.state, ClientState::Idle);
        assert_eq!(client.send(&cmd), Err(nb::Error::WouldBlock));
        assert_eq!(client.state, ClientState::AwaitingResponse);

        assert_eq!(client.check_response(&cmd), Err(nb::Error::WouldBlock));

        p.enqueue(Ok(Vec::<u8, TestRxBufLen>::new())).unwrap();

        assert_eq!(client.state, ClientState::AwaitingResponse);

        assert_eq!(client.check_response(&cmd), Ok(NoResponse));
        assert_eq!(client.state, ClientState::Idle);
    }

    // Testing unsupported feature in form of vec deserialization
    #[test]
    #[ignore]
    fn response_vec() {
        let (mut client, mut p, _) = setup!(Config::new(Mode::Blocking));

        let cmd = TestRespVecCmd {
            fun: Functionality::APM,
            rst: Some(ResetMode::DontReset),
        };

        let response =
            Vec::<u8, TestRxBufLen>::from_slice(b"+CUN: 22,16,\"0123456789012345\"").unwrap();
        p.enqueue(Ok(response)).unwrap();

        assert_eq!(client.state, ClientState::Idle);

        assert_eq!(
            client.send(&cmd),
            Ok(TestResponseVec {
                socket: 22,
                length: 16,
                data: Vec::from_slice(b"0123456789012345").unwrap()
            })
        );
        assert_eq!(client.state, ClientState::Idle);
        assert_eq!(client.tx.s, String::<consts::U32>::from("AT+CFUN=4,0\r\n"));
    }
    // Test response containing string
    #[test]
    fn response_string() {
        let (mut client, mut p, _) = setup!(Config::new(Mode::Blocking));

        // String last
        let cmd = TestRespStringCmd {
            fun: Functionality::APM,
            rst: Some(ResetMode::DontReset),
        };

        let response =
            Vec::<u8, TestRxBufLen>::from_slice(b"+CUN: 22,16,\"0123456789012345\"").unwrap();
        p.enqueue(Ok(response)).unwrap();

        assert_eq!(client.state, ClientState::Idle);

        assert_eq!(
            client.send(&cmd),
            Ok(TestResponseString {
                socket: 22,
                length: 16,
                data: String::<consts::U64>::from("0123456789012345")
            })
        );
        assert_eq!(client.state, ClientState::Idle);

        // Mixed order for string
        let cmd = TestRespStringMixCmd {
            fun: Functionality::APM,
            rst: Some(ResetMode::DontReset),
        };

        let response =
            Vec::<u8, TestRxBufLen>::from_slice(b"+CUN: \"0123456789012345\",22,16").unwrap();
        p.enqueue(Ok(response)).unwrap();

        assert_eq!(
            client.send(&cmd),
            Ok(TestResponseStringMixed {
                socket: 22,
                length: 16,
                data: String::<consts::U64>::from("0123456789012345")
            })
        );
        assert_eq!(client.state, ClientState::Idle);
    }

    #[test]
    fn urc() {
        let (mut client, _, mut urc_p) = setup!(Config::new(Mode::NonBlocking));

        let response = Vec::<u8, TestRxBufLen>::from_slice(b"+UMWI: 0, 1").unwrap();
        urc_p.enqueue(response).unwrap();

        assert_eq!(client.state, ClientState::Idle);
        assert!(client.check_urc::<Urc>().is_some());
        assert_eq!(client.state, ClientState::Idle);
    }

    #[test]
    fn invalid_response() {
        let (mut client, mut p, _) = setup!(Config::new(Mode::Blocking));

        // String last
        let cmd = TestRespStringCmd {
            fun: Functionality::APM,
            rst: Some(ResetMode::DontReset),
        };

        let response = Vec::<u8, TestRxBufLen>::from_slice(b"+CUN: 22,16,22").unwrap();
        p.enqueue(Ok(response)).unwrap();

        assert_eq!(client.state, ClientState::Idle);
        assert_eq!(client.send(&cmd), Err(nb::Error::Other(Error::Parse)));
        assert_eq!(client.state, ClientState::Idle);
    }
}
