pub mod errors;
use streamhub::{
    define::{
        DataReceiver, DataSender, FrameData, FrameDataSender, Information, InformationSender,
        NotifyInfo, PacketDataSender, PublishType, PublisherInfo, StreamHubEvent,
        StreamHubEventSender, SubscribeType, SubscriberInfo, TStreamHandler,
    },
    errors::ChannelError,
    statistics::StreamStatistics,
    stream::StreamIdentifier,
    utils::{RandomDigitCount, Uuid},
};
use tokio::sync::Mutex;

use bytesio::bytesio::TNetIO;
use bytesio::bytesio::TcpIO;
use std::sync::Arc;
use tokio::net::TcpStream;

use super::http::define::http_method_name;
use super::http::{HttpRequest, HttpResponse, Marshal, Unmarshal};

use super::whip::handle_whip;
use async_trait::async_trait;
use byteorder::BigEndian;
use bytes::BytesMut;
use bytesio::bytes_reader::BytesReader;
use bytesio::bytes_writer::AsyncBytesWriter;
use errors::SessionError;
use errors::SessionErrorValue;
use http::StatusCode;
use tokio::sync::mpsc;
use webrtc::peer_connection::{sdp::session_description::RTCSessionDescription, RTCPeerConnection};

pub struct WebRTCServerSession {
    io: Arc<Mutex<Box<dyn TNetIO + Send + Sync>>>,
    reader: BytesReader,
    writer: AsyncBytesWriter,

    event_producer: StreamHubEventSender,
    stream_handler: Arc<WebRTCStreamHandler>,

    pub session_id: Option<Uuid>,
    pub http_request_data: Option<HttpRequest>,
    pub peer_connection: Option<Arc<RTCPeerConnection>>,
}

impl WebRTCServerSession {
    pub fn new(stream: TcpStream, event_producer: StreamHubEventSender) -> Self {
        let net_io: Box<dyn TNetIO + Send + Sync> = Box::new(TcpIO::new(stream));
        let io = Arc::new(Mutex::new(net_io));

        Self {
            io: io.clone(),
            reader: BytesReader::new(BytesMut::default()),
            writer: AsyncBytesWriter::new(io),
            event_producer,
            stream_handler: Arc::new(WebRTCStreamHandler::new()),
            session_id: None,
            http_request_data: None,
            peer_connection: None,
        }
    }

    pub async fn close_peer_connection(&self) -> Result<(), SessionError> {
        if let Some(pc) = &self.peer_connection {
            pc.close().await?;
        }
        Ok(())
    }

    pub async fn run(&mut self) -> Result<(), SessionError> {
        log::info!("read run 0");
        while self.reader.len() < 4 {
            let data = self.io.lock().await.read().await?;
            self.reader.extend_from_slice(&data[..]);
        }
        log::info!("read run 1");

        let request_data = self.reader.extract_remaining_bytes();

        if let Some(http_request) = HttpRequest::unmarshal(std::str::from_utf8(&request_data)?) {
            //POST /whip?app=live&stream=test HTTP/1.1
            let eles: Vec<&str> = http_request.path.splitn(2, '/').collect();
            let pars_map = &http_request.path_parameters_map;

            if eles.len() < 2 || pars_map.get("app").is_none() || pars_map.get("stream").is_none() {
                log::error!(
                    "WebRTCServerSession::run the http path is not correct: {}",
                    http_request.path
                );

                return Err(SessionError {
                    value: errors::SessionErrorValue::HttpRequestPathError,
                });
            }

            let t = eles[1];
            let app_name = pars_map.get("app").unwrap().clone();
            let stream_name = pars_map.get("stream").unwrap().clone();

            log::info!("1:{},2:{},3:{}", t, app_name, stream_name);

            match http_request.method.as_str() {
                http_method_name::POST => {
                    let sdp_data = if let Some(body) = http_request.body.as_ref() {
                        body
                    } else {
                        return Err(SessionError {
                            value: errors::SessionErrorValue::HttpRequestEmptySdp,
                        });
                    };
                    self.session_id = Some(Uuid::new(RandomDigitCount::Zero));

                    match t.to_lowercase().as_str() {
                        "whip" => {
                            let offer = RTCSessionDescription::offer(sdp_data.clone())?;
                            let path = format!(
                                "{}?{}&session_id={}",
                                http_request.path,
                                http_request.path_parameters.as_ref().unwrap(),
                                self.session_id.unwrap()
                            );
                            self.handle_whip(app_name, stream_name, path, offer).await?;
                        }
                        "whep" => {
                            self.handle_whep();
                        }
                        _ => {
                            log::error!(
                                "current path: {}, method: {}",
                                http_request.path,
                                t.to_lowercase()
                            );
                            return Err(SessionError {
                                value: errors::SessionErrorValue::HttpRequestNotSupported,
                            });
                        }
                    }
                }
                http_method_name::OPTIONS => {}
                http_method_name::PATCH => {}
                http_method_name::DELETE => {
                    if let Some(session_id) = pars_map.get("session_id") {
                        if let Some(uuid) = Uuid::from_str2(session_id) {
                            self.session_id = Some(uuid);
                        }
                    } else {
                        log::error!(
                            "the delete path does not contain session id: {}?{}",
                            http_request.path,
                            http_request.path_parameters.as_ref().unwrap()
                        );
                    }
                }
                _ => {
                    log::warn!(
                        "WebRTCServerSession::unsupported method name: {}",
                        http_request.method
                    );
                }
            }

            self.http_request_data = Some(http_request);
        }

        Ok(())
    }

    async fn handle_whip(
        &mut self,
        app_name: String,
        stream_name: String,
        path: String,
        offer: RTCSessionDescription,
    ) -> Result<(), SessionError> {
        // The sender is used for sending audio/video frame data to the stream hub
        // receiver is passed to the stream hub for receiving the a/v packet data
        let (sender, receiver) = mpsc::unbounded_channel();
        let (_, no_used_receiver) = mpsc::unbounded_channel();
        let publish_event = StreamHubEvent::Publish {
            identifier: StreamIdentifier::WebRTC {
                app_name,
                stream_name,
            },
            receiver: DataReceiver {
                packet_receiver: receiver,
                frame_receiver: no_used_receiver,
            },
            info: self.get_publisher_info(),
            stream_handler: self.stream_handler.clone(),
        };

        if self.event_producer.send(publish_event).is_err() {
            return Err(SessionError {
                value: SessionErrorValue::StreamHubEventSendErr,
            });
        }

        let response = match handle_whip(offer, sender).await {
            Ok((session_description, peer_connection)) => {
                self.peer_connection = Some(peer_connection);

                let status_code = http::StatusCode::CREATED;
                let mut response = Self::gen_response(status_code);

                response
                    .headers
                    .insert("Connection".to_string(), "Close".to_string());
                response
                    .headers
                    .insert("Content-Type".to_string(), "application/sdp".to_string());
                response.headers.insert("Location".to_string(), path);
                response.body = Some(session_description.sdp);

                response
            }
            Err(err) => {
                log::error!("handle whip err: {}", err);
                let status_code = http::StatusCode::SERVICE_UNAVAILABLE;
                Self::gen_response(status_code)
            }
        };

        self.send_response(&response).await
    }

    fn handle_whep(&self) {}

    fn get_publisher_info(&self) -> PublisherInfo {
        let id = if let Some(session_id) = &self.session_id {
            *session_id
        } else {
            Uuid::new(RandomDigitCount::Zero)
        };

        PublisherInfo {
            id,
            pub_type: PublishType::PushWebRTC,
            notify_info: NotifyInfo {
                request_url: String::from(""),
                remote_addr: String::from(""),
            },
        }
    }

    fn gen_response(status_code: StatusCode) -> HttpResponse {
        let reason_phrase = if let Some(reason) = status_code.canonical_reason() {
            reason.to_string()
        } else {
            "".to_string()
        };

        HttpResponse {
            version: "HTTP/1.1".to_string(),
            status_code: status_code.as_u16(),
            reason_phrase,
            ..Default::default()
        }
    }

    async fn send_response(&mut self, response: &HttpResponse) -> Result<(), SessionError> {
        self.writer.write(response.marshal().as_bytes())?;
        self.writer.flush().await?;

        Ok(())
    }
}

#[derive(Default)]
pub struct WebRTCStreamHandler {}

impl WebRTCStreamHandler {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl TStreamHandler for WebRTCStreamHandler {
    async fn send_prior_data(
        &self,
        sender: DataSender,
        sub_type: SubscribeType,
    ) -> Result<(), ChannelError> {
        Ok(())
    }
    async fn get_statistic_data(&self) -> Option<StreamStatistics> {
        None
    }

    async fn send_information(&self, sender: InformationSender) {}
}
