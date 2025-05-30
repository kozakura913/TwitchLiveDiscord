use std::{
	collections::{HashMap, HashSet},
	path::Path,
	time::Duration,
};
/*
 * SPDX-FileCopyrightText: 2025 Kozakura913 <kozakura@kzkr.xyz>
 * SPDX-License-Identifier: AGPL-3.0-only
 * このソフトウェアはAGPL3.0ライセンスに従い利用することができます
 * 作者連絡先
 *  https://github.com/kozakura913
 *  https://kzkr.xyz/profile
 *  https://xn--vusz0j.art/@kozakura
*/
use chrono::{DateTime, Utc};
use reqwest::{
	Client, RequestBuilder,
	header::{CONTENT_TYPE, HeaderMap},
	multipart,
};
use serde::{Deserialize, Serialize};
use tokio_util::bytes::Bytes;

fn main() {
	tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap()
		.block_on(async_exec());
}
#[derive(Clone, Default, Debug, PartialEq, Serialize, Deserialize)]
struct State {
	auth: Option<OAuthResponse>,
	lives: Option<ResponseList<LiveStream>>,
}
impl State {
	fn read() -> Option<Self> {
		let state_file = std::fs::File::open("state.json").ok()?;
		serde_json::from_reader(state_file).ok()
	}
	fn write(&self) {
		let state_file = std::fs::File::create("state.json");
		if let Ok(f) = state_file {
			if let Err(e) = serde_json::to_writer_pretty(f, self) {
				eprintln!("write state file {:?}", e);
			}
		}
	}
	fn trim(&self, stream_list: &mut ResponseList<LiveStream>) {
		let lives = match &self.lives {
			Some(v) => v,
			None => return,
		};
		let lives = lives.data.iter().map(|s| &s.id);
		let mut set = HashSet::new();
		for id in lives {
			set.insert(id);
		}
		stream_list.data.retain(|stream| !set.contains(&stream.id));
	}
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ConfigFile {
	client_id: String,
	client_secret: String,
	target_user: String,
	discord: String,
}
async fn async_exec() {
	if !std::fs::exists("config.json").unwrap() {
		let mut f = std::fs::File::create_new("config.json").expect("create example config.json");
		serde_json::to_writer_pretty(
			&mut f,
			&ConfigFile {
				client_id: "".into(),
				client_secret: "".into(),
				target_user: "".into(),
				discord: "https://discord.com/api/webhooks/".into(),
			},
		)
		.expect("example config.json");
		println!("create new config.json");
		return;
	}
	let config = std::fs::File::open("config.json").expect("config.json read error");
	let config: ConfigFile = serde_json::from_reader(&config).expect("parse config.json");
	let client = reqwest::ClientBuilder::new().build().unwrap();
	let mut state = State::read();

	let auth = match state.as_ref().map(|c| c.auth.clone()) {
		Some(Some(auth)) => auth,
		_ => login(&client, &config).await,
	};
	let api = TwitchAPI::new(auth, client.clone(), config.client_id.clone());
	let streams = api.get_streams_by_name(&config.target_user).await;
	println!("{:?}", streams);
	let e = match streams {
		Ok(mut stream_list) => {
			State {
				auth: Some(api.auth.clone()),
				lives: Some(stream_list.clone()),
			}
			.write();
			if let Some(state) = &state {
				state.trim(&mut stream_list);
			}
			build_message_and_send(&client, &config, stream_list).await;
			//一発成功
			return;
		}
		Err(e) => e,
	};
	//とりあえず失敗
	if let TwitchAPIError::Reqwest(e) = e {
		if let Some(status) = e.status() {
			if status.as_u16() != 401 {
				//401はセッション切れの場合がある。他は謎だから無視
				eprintln!("{:?}", e);
				return;
			}
		}
	}
	if let Some(state) = state.as_mut() {
		//セッション情報があればもう使えないので破棄
		if state.auth.is_some() {
			state.auth = None;
			state.write();
		}
	}
	let auth = login(&client, &config).await;
	let mut state = state.unwrap_or_default();
	state.auth = Some(auth.clone());
	//ログインできたらセッション情報を保存しておく
	state.write();
	//2回目
	let api = TwitchAPI::new(auth, client.clone(), config.client_id.clone());
	let streams = api.get_streams_by_name(&config.target_user).await;
	println!("{:?}", streams);
	match streams {
		Ok(mut stream_list) => {
			State {
				auth: Some(api.auth.clone()),
				lives: Some(stream_list.clone()),
			}
			.write();
			state.trim(&mut stream_list);
			build_message_and_send(&client, &config, stream_list).await;
			//2回目で成功
			return;
		}
		Err(e) => {
			eprintln!("{:?}", e);
		}
	};
	//println!("{:?}",api.get_user_id(&target_user).await);
}
async fn get_thumbnail_image(client: &Client, stream: &LiveStream) -> Option<(HeaderMap, Bytes)> {
	let base_url = stream.thumbnail_url.as_ref()?;
	let thumbnail_url = base_url.replace("{width}", "0").replace("{height}", "0");
	let request = client
		.get(thumbnail_url)
		.timeout(Duration::from_secs(5))
		.build()
		.map_err(|e| eprintln!("{}:{} {:?}", file!(), line!(), e))
		.ok()?;
	let response = client
		.execute(request)
		.await
		.map_err(|e| eprintln!("{}:{} {:?}", file!(), line!(), e))
		.ok()?;
	let headers = response.headers().clone();
	let bytes = response
		.bytes()
		.await
		.map_err(|e| eprintln!("{}:{} {:?}", file!(), line!(), e))
		.ok()?;
	Some((headers, bytes))
}
async fn build_message_and_send(
	client: &Client,
	config: &ConfigFile,
	stream_list: ResponseList<LiveStream>,
) {
	if stream_list.data.is_empty() {
		return;
	}
	let thumbnail_images = {
		let get_thumbnail_image_job = stream_list
			.data
			.iter()
			.map(|stream| get_thumbnail_image(client, stream));
		let thumbnail_images = futures_util::future::join_all(get_thumbnail_image_job).await;
		let mut map = HashMap::new();
		let images = stream_list.data.iter().zip(thumbnail_images.into_iter());
		for (stream, img) in images {
			if let Some(img) = img {
				let name = stream.thumbnail_name();
				map.insert(stream.id.clone(), (img, name));
			}
		}
		map
	};
	let mut embeds = Vec::new();
	for stream in stream_list.data.iter() {
		embeds.push(DiscordHookEmbed {
			title: stream
				.title
				.clone()
				.unwrap_or_else(|| stream.game_name.clone().unwrap_or("タイトル不明".into())),
			url: Some(format!(
				"https://www.twitch.tv/{}",
				stream.user_login.as_str()
			)),
			description: stream.game_name.clone(),
			timestamp: Some(stream.started_at),
			image: thumbnail_images
				.get(stream.id.as_str())
				.map(|(_, name)| DiscordHookEmbedImage { url: format!("attachment://{}",&name) }),
			thumbnail: None,
			color: None,
		});
	}
	let multipart_form = if !thumbnail_images.is_empty() {
		let mut form = multipart::Form::new();
		for ((header, b), name) in thumbnail_images.into_values() {
			let len = b.len() as u64;
			let part = multipart::Part::stream_with_length(b, len).file_name(name.clone());
			if let Some(Ok(mime)) = header.get(CONTENT_TYPE).map(|mime| mime.to_str()) {
				if let Ok(part) = part.mime_str(mime) {
					form = form.part(name, part);
				}
			} else {
				form = form.part(name, part);
			}
		}
		Some(form)
	} else {
		None
	};
	let body = serde_json::to_string(&DiscordHookBody {
		avatar_url: None, //未使用
		content: "生放送が開始されました".into(),
		embeds,
	})
	.unwrap();
	let request = match multipart_form {
		Some(form) => {
			let form = form.text("payload_json", body);
			client.post(&config.discord).multipart(form).build()
		}
		None => client
			.post(&config.discord)
			.header(CONTENT_TYPE, "application/json")
			.body(body)
			.build(),
	}
	.unwrap();
	let response = client.execute(request).await.unwrap();
	println!("send discord {}", response.status());
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct DiscordHookBody {
	avatar_url: Option<String>,
	content: String,
	embeds: Vec<DiscordHookEmbed>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct DiscordHookEmbed {
	title: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	description: Option<String>,
	url: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	timestamp: Option<DateTime<Utc>>,
	#[serde(skip_serializing_if = "Option::is_none")]
	color: Option<i32>,
	#[serde(skip_serializing_if = "Option::is_none")]
	image: Option<DiscordHookEmbedImage>,
	#[serde(skip_serializing_if = "Option::is_none")]
	thumbnail: Option<DiscordHookEmbedImage>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct DiscordHookEmbedImage {
	url: String,
}
async fn login(client: &Client, config: &ConfigFile) -> OAuthResponse {
	println!("login...");
	let client_id = config.client_id.as_str();
	let client_secret = config.client_secret.as_str();
	let body = format!(
		"client_id={client_id}&client_secret={client_secret}&grant_type=client_credentials"
	);
	let request = client.post("https://id.twitch.tv/oauth2/token");
	let request = request.header(
		"Content-Type",
		"application/x-www-form-urlencoded; charset=UTF-8",
	);
	let request = request.header("Content-Length", body.bytes().len());
	let request = request.body(body);
	let request = request.build().unwrap();
	let res = client.execute(request).await.expect("oauth2");
	let res = res.bytes().await.expect("oauth2");
	match serde_json::from_slice::<OAuthResponse>(&res[..]) {
		Ok(res) => {
			println!("{:?}", res);
			res
		}
		Err(e) => {
			println!("{:?}", e);
			println!("{:?}", String::from_utf8(Vec::from(&res[..])));
			panic!()
		}
	}
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct OAuthResponse {
	access_token: String,
	expires_in: i64,
	token_type: String,
}
struct TwitchAPI {
	auth: OAuthResponse,
	client: Client,
	client_id: String,
}
#[derive(Debug)]
enum TwitchAPIError {
	Reqwest(reqwest::Error),
	Json(serde_json::Error),
}
impl From<reqwest::Error> for TwitchAPIError {
	fn from(value: reqwest::Error) -> Self {
		Self::Reqwest(value)
	}
}
impl From<serde_json::Error> for TwitchAPIError {
	fn from(value: serde_json::Error) -> Self {
		Self::Json(value)
	}
}
impl TwitchAPI {
	fn new(auth: OAuthResponse, client: Client, client_id: String) -> Self {
		Self {
			auth,
			client,
			client_id,
		}
	}
	fn add_headers(&self, req: RequestBuilder) -> RequestBuilder {
		let req = req.header("Client-ID", &self.client_id);
		let req = req.header(
			"Authorization",
			format!("Bearer {}", self.auth.access_token),
		);
		req
	}
	async fn get_user_id(
		&self,
		username: &str,
	) -> Result<ResponseList<UserProfile>, TwitchAPIError> {
		let req = self.client.get(format!(
			"https://api.twitch.tv/helix/users?login={username}"
		));
		let req = self.add_headers(req);
		let req = req.build()?;
		let res = self.client.execute(req).await?;
		let res = res.bytes().await?;
		match serde_json::from_slice(&res[..]) {
			Ok(res) => Ok(res),
			Err(e) => {
				println!("{:?}", e);
				println!("{:?}", String::from_utf8(Vec::from(&res[..])));
				Err(TwitchAPIError::Json(e))
			}
		}
	}
	async fn get_streams_by_name(
		&self,
		username: &str,
	) -> Result<ResponseList<LiveStream>, TwitchAPIError> {
		let req = self.client.get(format!(
			"https://api.twitch.tv/helix/streams?user_login={}",
			username
		));
		let req = self.add_headers(req);
		let req = req.build()?;
		let res = self.client.execute(req).await?;
		let res = res.bytes().await?;
		match serde_json::from_slice(&res[..]) {
			Ok(res) => Ok(res),
			Err(e) => {
				println!("{:?}", e);
				println!("{:?}", String::from_utf8(Vec::from(&res[..])));
				Err(TwitchAPIError::Json(e))
			}
		}
	}
	async fn get_streams_by_profile(
		&self,
		user: &UserProfile,
	) -> Result<ResponseList<LiveStream>, TwitchAPIError> {
		let req = self.client.get(format!(
			"https://api.twitch.tv/helix/streams?user_id={}",
			user.id
		));
		let req = self.add_headers(req);
		let req = req.build()?;
		let res = self.client.execute(req).await?;
		let res = res.bytes().await?;
		match serde_json::from_slice(&res[..]) {
			Ok(res) => Ok(res),
			Err(e) => {
				println!("{:?}", e);
				println!("{:?}", String::from_utf8(Vec::from(&res[..])));
				Err(TwitchAPIError::Json(e))
			}
		}
	}
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct ResponseList<T> {
	data: Vec<T>,
}
impl<T> ResponseList<T> {
	fn new() -> Self {
		Self { data: Vec::new() }
	}
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct UserProfile {
	id: String,
	login: String,
	display_name: Option<String>,
	r#type: Option<String>,
	broadcaster_type: Option<String>,
	description: Option<String>,
	profile_image_url: Option<String>,
	offline_image_url: Option<String>,
	email: Option<String>,
	view_count: Option<i64>,
	created_at: DateTime<Utc>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct LiveStream {
	//0000
	id: String,
	//0000
	user_id: String,
	//exampleuser
	user_login: String,
	//ExampleUser
	user_name: Option<String>,
	//0000
	game_id: Option<String>,
	//GameTitle
	game_name: Option<String>,
	//live
	r#type: String,
	//Live Stream Title
	title: Option<String>,
	//0
	viewer_count: i64,
	//2021-03-10T03:18:11Z
	started_at: DateTime<Utc>,
	thumbnail_url: Option<String>,
	//成人向け？
	is_mature: bool,
}
impl LiveStream {
	fn thumbnail_name(&self) -> String {
		let mut extension = "jpg";
		if let Some(name) = self.thumbnail_url.as_ref() {
			if let Some(Some(ext)) = Path::new(name).extension().map(|s| s.to_str()) {
				if !ext.is_empty() {
					extension = ext;
				}
			}
		};
		format!("{}.{}", &self.id, extension)
	}
}
