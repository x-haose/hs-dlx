#include "hs-dlx-torrent/src/shim.h"
#include "hs-dlx-torrent/src/lib.rs.h"

#include <algorithm>
#include <chrono>
#include <filesystem>
#include <stdexcept>

#include <libtorrent/add_torrent_params.hpp>
#include <libtorrent/alert_types.hpp>
#include <libtorrent/create_torrent.hpp>
#include <libtorrent/error_code.hpp>
#include <libtorrent/fingerprint.hpp>
#include <libtorrent/hex.hpp>
#include <libtorrent/load_torrent.hpp>
#include <libtorrent/magnet_uri.hpp>
#include <libtorrent/read_resume_data.hpp>
#include <libtorrent/session_params.hpp>
#include <libtorrent/session_stats.hpp>
#include <libtorrent/settings_pack.hpp>
#include <libtorrent/torrent_status.hpp>
#include <libtorrent/write_resume_data.hpp>

namespace hsdlx {

namespace {

std::string string_of(rust::Str text) { return std::string(text.data(), text.size()); }

// libtorrent 给的文字（种子名、文件路径、说明）不保证是 UTF-8（磁力链接的 dn、种子里的路径都是对方写的）：不是 UTF-8 的
// 字节换成替换字符
rust::String text_of(std::string const& text) { return rust::String::lossy(text); }

// 加入时定下的编号：有 v1 信息哈希时为它，否则为截到 20 字节的 v2 的，都写成十六进制
std::string canonical(lt::info_hash_t const& hashes) {
	return lt::aux::to_hex(hashes.has_v1() ? hashes.v1 : hashes.get(lt::protocol_version::V2));
}

// `id` 是 `hashes` 里 v1 或（截短的）v2 信息哈希的十六进制串
bool names(lt::info_hash_t const& hashes, std::string const& id) {
	return (hashes.has_v1() && lt::aux::to_hex(hashes.v1) == id)
		|| (hashes.has_v2() && lt::aux::to_hex(hashes.get(lt::protocol_version::V2)) == id);
}

// 照 `proxy` 设代理：节点、tracker 都经它，主机名由它解析；`host` 为空时不经代理
void set_proxy_of(lt::settings_pack& pack, SocksProxy const& proxy) {
	bool const on = !proxy.host.empty();
	bool const login = on && !proxy.username.empty();
	pack.set_int(lt::settings_pack::proxy_type,
		!on ? lt::settings_pack::none : login ? lt::settings_pack::socks5_pw : lt::settings_pack::socks5);
	pack.set_str(lt::settings_pack::proxy_hostname, std::string(proxy.host));
	pack.set_int(lt::settings_pack::proxy_port, proxy.port);
	pack.set_str(lt::settings_pack::proxy_username, std::string(proxy.username));
	pack.set_str(lt::settings_pack::proxy_password, std::string(proxy.password));
	pack.set_bool(lt::settings_pack::proxy_peer_connections, on);
	pack.set_bool(lt::settings_pack::proxy_tracker_connections, on);
	pack.set_bool(lt::settings_pack::proxy_hostnames, on);
}

lt::settings_pack settings_of(SessionConfig const& config) {
	lt::settings_pack pack;
	pack.set_str(lt::settings_pack::listen_interfaces, string_of(config.listen_interfaces));
	pack.set_bool(lt::settings_pack::enable_dht, config.dht);
	if (!config.dht_bootstrap_nodes.empty()) {
		pack.set_str(lt::settings_pack::dht_bootstrap_nodes, string_of(config.dht_bootstrap_nodes));
	}
	pack.set_bool(lt::settings_pack::enable_upnp, config.upnp);
	pack.set_bool(lt::settings_pack::enable_natpmp, config.natpmp);
	pack.set_bool(lt::settings_pack::enable_lsd, config.lsd);
	pack.set_str(lt::settings_pack::user_agent, string_of(config.user_agent));
	pack.set_str(lt::settings_pack::peer_fingerprint, string_of(config.peer_fingerprint));
	pack.set_int(lt::settings_pack::upload_rate_limit, config.upload_limit);
	set_proxy_of(pack, config.proxy);
	// 协议加密：两个方向都允许，优先加密，对方只会明文时照样连
	pack.set_int(lt::settings_pack::out_enc_policy, lt::settings_pack::pe_enabled);
	pack.set_int(lt::settings_pack::in_enc_policy, lt::settings_pack::pe_enabled);
	pack.set_int(lt::settings_pack::allowed_enc_level, lt::settings_pack::pe_both);
	pack.set_bool(lt::settings_pack::prefer_rc4, true);
	// 关会话时向 tracker 报告停止最多等这么久（秒）：释放会话要等它，默认 5 秒，退出应用时界面一直等着
	pack.set_int(lt::settings_pack::stop_tracker_timeout, 2);
	// 每层 tracker 各通告一个（uTorrent 的做法）：种子自己的 tracker 能用时，补在后面各层的照样通告；默认按多 tracker
	// 规范只用最前面能用的那层。同一层里有了能用的，其余的不再通告（torrent.cpp 的 announce_with_tracker），所以补的各占
	// 一层
	pack.set_bool(lt::settings_pack::announce_to_all_tiers, true);
	// 只要出错、状态、存储与端口映射的事件：tracker 与节点的事件量大；诊断要的统计另由 post_session_stats 要（它的事件
	// 不受这个设置管）
	pack.set_int(lt::settings_pack::alert_mask,
		static_cast<int>(static_cast<std::uint32_t>(lt::alert_category::error
			| lt::alert_category::status
			| lt::alert_category::storage
			| lt::alert_category::port_mapping)));
	return pack;
}

Alert alert_of(AlertKind kind, lt::alert const& alert, std::string const& torrent) {
	Alert out;
	out.kind = kind;
	out.torrent = rust::String(torrent);
	out.message = text_of(alert.message());
	return out;
}

} // namespace

namespace {

// 会话的参数：`config.state` 为上次 session_state() 写出的（只取 DHT 的状态），空时从头开始；设置一律取 `config`
lt::session_params params_of(SessionConfig const& config) {
	lt::session_params params;
	if (!config.state.empty()) {
		params = lt::read_session_params(
			lt::span<char const>(reinterpret_cast<char const*>(config.state.data()), static_cast<std::ptrdiff_t>(config.state.size())),
			lt::session::save_dht_state);
	}
	params.settings = settings_of(config);
	return params;
}

lt::span<char const> bytes_of(rust::Slice<std::uint8_t const> data) {
	return lt::span<char const>(reinterpret_cast<char const*>(data.data()), static_cast<std::ptrdiff_t>(data.size()));
}

rust::Vec<std::uint8_t> bytes_out(std::vector<char> const& data) {
	rust::Vec<std::uint8_t> out;
	out.reserve(data.size());
	for (char c : data) out.push_back(static_cast<std::uint8_t>(c));
	return out;
}

} // namespace

Session::Session(SessionConfig const& config)
	: m_session(params_of(config))
{}

std::unique_ptr<Session> new_session(SessionConfig const& config) {
	return std::make_unique<Session>(config);
}

rust::String Session::add(lt::add_torrent_params params, std::string const& id, rust::Str save_path) const {
	params.save_path = string_of(save_path);
	// 排队、开停由应用决定，libtorrent 不自动开停（默认的标志是「暂停 + 自动管理」，由它来开）；已在会话里的报错，不返回
	// 原有的那个
	params.flags &= ~lt::torrent_flags::auto_managed;
	params.flags |= lt::torrent_flags::duplicate_is_error;
	std::lock_guard<std::mutex> lock(m_mutex);
	if (still_removing(id)) throw std::invalid_argument("这个种子还在移除中，移除完才能再加：" + id);
	lt::torrent_handle h = m_session.add_torrent(std::move(params));
	m_handles[id] = h;
	m_ids[h] = id;
	m_added_trackers.erase(id);
	return rust::String(id);
}

lt::torrent_handle Session::handle(rust::Str id) const {
	std::lock_guard<std::mutex> lock(m_mutex);
	auto found = m_handles.find(string_of(id));
	if (found == m_handles.end()) throw std::invalid_argument("没有这个种子：" + string_of(id));
	return found->second;
}

std::string Session::id_of(lt::torrent_handle const& handle) const {
	std::lock_guard<std::mutex> lock(m_mutex);
	auto found = m_ids.find(handle);
	if (found == m_ids.end()) throw std::logic_error("事件里的种子不是经这个会话加入的");
	return found->second;
}

rust::String magnet_id(rust::Str uri) {
	return rust::String(canonical(lt::parse_magnet_uri(string_of(uri)).info_hashes));
}

rust::String Session::add_magnet(rust::Str uri, rust::Str save_path) const {
	lt::add_torrent_params params = lt::parse_magnet_uri(string_of(uri));
	// 不暂停（连节点、取种子信息），但只上传不下载：选好文件后 start 才开始下
	params.flags &= ~lt::torrent_flags::paused;
	params.flags |= lt::torrent_flags::upload_mode;
	std::string const id = canonical(params.info_hashes);
	return add(std::move(params), id, save_path);
}

rust::String Session::add_resume(rust::Slice<std::uint8_t const> resume, rust::Str id, rust::Str save_path,
	AddOptions const& options) const {
	lt::add_torrent_params params = lt::read_resume_data(bytes_of(resume));
	lt::info_hash_t const hashes = params.ti ? params.ti->info_hashes() : params.info_hashes;
	std::string const expected = string_of(id);
	if (!names(hashes, expected)) throw std::invalid_argument("续传数据不是这个种子的：" + expected);
	if (!options.trust_pieces) {
		// 没有已有块的记录时，libtorrent 发现磁盘上有这个种子的文件就整体校验（pread_disk_io.cpp 与 mmap_disk_io.cpp 的
		// do_check_fastresume）
		params.have_pieces.clear();
		params.verified_pieces.clear();
		params.unfinished_pieces.clear();
		params.flags &= ~lt::torrent_flags::seed_mode;
	}
	// 写盘出错时 libtorrent 改成的只上传随续传数据存下，再加入即是要再试；做种时另要只上传
	if (options.upload_only) {
		params.flags |= lt::torrent_flags::upload_mode;
	} else {
		params.flags &= ~lt::torrent_flags::upload_mode;
	}
	if (options.paused) {
		params.flags |= lt::torrent_flags::paused;
	} else {
		params.flags &= ~lt::torrent_flags::paused;
	}
	return add(std::move(params), expected, save_path);
}

bool Session::removing(rust::Str id) const {
	std::lock_guard<std::mutex> lock(m_mutex);
	return still_removing(string_of(id));
}

void Session::mark_lost() const {
	std::lock_guard<std::mutex> lock(m_mutex);
	for (auto& entry : m_removing) entry.second.lost = true;
}

bool Session::current(std::string const& id, lt::torrent_handle const& handle) const {
	std::lock_guard<std::mutex> lock(m_mutex);
	auto const live = m_handles.find(id);
	if (live != m_handles.end() && live->second == handle) return true;
	auto const leaving = m_removing.find(id);
	return leaving != m_removing.end() && leaving->second.handle == handle;
}

bool Session::still_removing(std::string const& id) const {
	auto found = m_removing.find(id);
	if (found == m_removing.end()) return false;
	if (!found->second.lost || found->second.handle.is_valid()) return true;
	m_removing.erase(found);
	return false;
}

void Session::start(rust::Str id) const { handle(id).unset_flags(lt::torrent_flags::upload_mode); }

void Session::flush(rust::Str id) const {
	// 与 remove 同一把锁：两者按调用的先后交给网络线程，先交的写盘在种子移除前做完、照样报来
	std::lock_guard<std::mutex> lock(m_mutex);
	std::string const key = string_of(id);
	auto found = m_handles.find(key);
	if (found == m_handles.end()) throw std::invalid_argument("没有这个种子：" + key);
	// 没有种子信息时还没有存储，libtorrent 不发写完的事件
	if (!found->second.torrent_file()) throw std::invalid_argument("还没有种子信息，没有要写到磁盘的：" + key);
	found->second.flush_cache();
	++m_flush_requests[key];
}

bool Session::flush_requested(std::string const& id) const {
	std::lock_guard<std::mutex> lock(m_mutex);
	auto found = m_flush_requests.find(id);
	if (found == m_flush_requests.end()) return false;
	if (--found->second == 0) m_flush_requests.erase(found);
	return true;
}

rust::Vec<std::uint8_t> Session::session_state() const {
	return bytes_out(lt::write_session_params_buf(m_session.session_state(lt::session::save_dht_state)));
}

Status Session::status(rust::Str id) const {
	lt::torrent_handle h = handle(id);
	lt::torrent_status s = h.status(lt::torrent_handle::query_pieces | lt::torrent_handle::query_name);
	Status out;
	out.state = static_cast<std::uint8_t>(s.state);
	out.progress_ppm = s.progress_ppm;
	out.has_metadata = s.has_metadata;
	out.paused = static_cast<bool>(s.flags & lt::torrent_flags::paused);
	out.held = static_cast<bool>(s.flags & lt::torrent_flags::upload_mode);
	out.name = text_of(s.name);
	out.total_wanted = s.total_wanted;
	out.total_wanted_done = s.total_wanted_done;
	out.all_time_upload = s.all_time_upload;
	out.finished_seconds = s.finished_duration.count();
	out.finished = s.is_finished;
	std::shared_ptr<lt::torrent_info const> const info = h.torrent_file();
	out.private_torrent = info && info->priv();
	out.swarm_seeds = s.num_complete;
	out.swarm_leechers = s.num_incomplete;
	out.downloaded = s.total_payload_download;
	out.download_rate = s.download_payload_rate;
	out.upload_rate = s.upload_payload_rate;
	out.peers = s.num_peers;
	out.seeds = s.num_seeds;
	out.error = text_of(s.errc ? s.errc.message() : std::string());
	// 要下的块（选了的文件所在的块），其中能凑齐的：自己已有的，或在线的节点里至少有一个有的
	std::vector<int> availability;
	std::vector<lt::download_priority_t> priorities;
	if (s.has_metadata) {
		h.piece_availability(availability);
		priorities = h.get_piece_priorities();
	}
	std::int32_t wanted = 0;
	std::int32_t reachable = 0;
	for (std::size_t i = 0; i < priorities.size() && i < std::size_t(s.pieces.size()); ++i) {
		if (priorities[i] == lt::dont_download) continue;
		++wanted;
		bool const available = i < availability.size() && availability[i] > 0;
		if (s.pieces.get_bit(lt::piece_index_t(static_cast<int>(i))) || available) ++reachable;
	}
	out.pieces = wanted;
	out.reachable_pieces = reachable;
	return out;
}

void Session::connect_peer(rust::Str id, rust::Str ip, std::uint16_t port) const {
	boost::system::error_code ec;
	auto address = boost::asio::ip::make_address(string_of(ip), ec);
	if (ec) throw std::invalid_argument("不是 IP 地址：" + string_of(ip));
	handle(id).connect_peer(lt::tcp::endpoint(address, port));
}

void Session::pause(rust::Str id) const { handle(id).pause(lt::torrent_handle::graceful_pause); }

void Session::resume(rust::Str id) const { handle(id).resume(); }

void Session::remove(rust::Str id, Removal removal) const {
	// 整个过程持有锁：别的线程不能在这期间加入同一个种子
	std::lock_guard<std::mutex> lock(m_mutex);
	std::string const key = string_of(id);
	auto found = m_handles.find(key);
	if (found == m_handles.end()) throw std::invalid_argument("没有这个种子：" + key);
	lt::remove_flags_t flags{};
	if (removal == Removal::Files) flags = lt::session::delete_files;
	m_session.remove_torrent(found->second, flags);
	// 删文件时 libtorrent 另发删的结果（torrent.cpp 的 on_files_deleted）
	m_removing[key] = Removing{found->second, false, flags != lt::remove_flags_t{}, false, false};
	m_handles.erase(found);
}

void Session::removed(std::string const& id, bool files_settled) const {
	std::lock_guard<std::mutex> lock(m_mutex);
	auto found = m_removing.find(id);
	if (found == m_removing.end()) return;
	Removing& removing = found->second;
	if (files_settled) {
		removing.files_settled = true;
	} else {
		removing.removed = true;
	}
	// 移除的事件与（要删文件时）删的结果都来了才算移除完，两者先后不定
	if (removing.removed && (!removing.deleting || removing.files_settled)) m_removing.erase(found);
}

void Session::save_resume_data(rust::Str id) const {
	handle(id).save_resume_data(lt::torrent_handle::save_info_dict);
}

std::uint16_t Session::listen_port() const { return m_session.listen_port(); }

NetworkStatus Session::network() const {
	std::lock_guard<std::mutex> lock(m_network_mutex);
	NetworkStatus out;
	out.listen_port = m_session.listen_port();
	out.listen_error = text_of(m_network.listen_error);
	out.upnp_port = m_network.upnp_port;
	out.upnp_error = text_of(m_network.upnp_error);
	out.natpmp_port = m_network.natpmp_port;
	out.natpmp_error = text_of(m_network.natpmp_error);
	out.dht_nodes = m_network.dht_nodes;
	out.incoming = m_network.incoming;
	out.proxy_error = text_of(m_network.proxy_error);
	return out;
}

void Session::request_stats() const { m_session.post_session_stats(); }

rust::Vec<TrackerState> Session::trackers(rust::Str id) const {
	rust::Vec<TrackerState> out;
	std::string const key = string_of(id);
	for (lt::announce_entry const& entry : handle(id).trackers()) {
		TrackerState state;
		state.url = text_of(entry.url);
		state.added = added_tracker(key, entry.url);
		state.working = false;
		state.updating = false;
		state.seeds = -1;
		state.leechers = -1;
		// 各监听地址（IPv4、IPv6、回环）各通告一次，每次 v1 与 v2 各一：有一次成功就算正常；每个地址都失败过才算连不上
		// （路由不到 tracker 的地址立即以跳过失败，别的还在等）；人数取最多的。说明取第一条，跳过的说明只在没有别的时取：
		// 回环地址对任何外面的 tracker 都跳过，它的说明盖住真正的原因
		bool every_endpoint_failed = !entry.endpoints.empty();
		std::string message;
		std::string skipped;
		for (lt::announce_endpoint const& endpoint : entry.endpoints) {
			bool endpoint_failed = false;
			for (lt::announce_infohash const& ih : endpoint.info_hashes) {
				if (ih.start_sent && ih.fails == 0) state.working = true;
				if (ih.fails > 0) endpoint_failed = true;
				if (ih.updating) state.updating = true;
				state.seeds = std::max(state.seeds, ih.scrape_complete);
				state.leechers = std::max(state.leechers, ih.scrape_incomplete);
				std::string const said = !ih.message.empty() ? ih.message
					: ih.last_error ? ih.last_error.message() : std::string();
				std::string& into = ih.last_error == lt::errors::announce_skipped ? skipped : message;
				if (into.empty()) into = said;
			}
			if (!endpoint_failed) every_endpoint_failed = false;
		}
		state.message = text_of(message.empty() ? skipped : message);
		state.failed = !state.working && every_endpoint_failed;
		out.push_back(std::move(state));
	}
	return out;
}

std::uint32_t Session::add_trackers(rust::Str id, rust::Slice<rust::String const> urls) const {
	std::lock_guard<std::mutex> lock(m_mutex);
	std::string const key = string_of(id);
	auto found = m_handles.find(key);
	if (found == m_handles.end()) throw std::invalid_argument("没有这个种子：" + key);
	std::vector<lt::announce_entry> const present = found->second.trackers();
	// 补在已有的之后，各占一层（层号最大 255，超出的同在 255 层）
	int tier = 0;
	for (lt::announce_entry const& entry : present) tier = std::max(tier, int(entry.tier) + 1);
	std::set<std::string>& added = m_added_trackers[key];
	std::uint32_t count = 0;
	for (rust::String const& url : urls) {
		std::string const text(url.data(), url.size());
		bool const has = added.count(text) > 0 || std::any_of(present.begin(), present.end(),
			[&](lt::announce_entry const& entry) { return entry.url == text; });
		if (has) continue;
		lt::announce_entry entry(text);
		entry.tier = static_cast<std::uint8_t>(std::min(tier, 255));
		found->second.add_tracker(entry);
		added.insert(text);
		++count;
		++tier;
	}
	return count;
}

bool Session::added_tracker(std::string const& id, std::string const& url) const {
	std::lock_guard<std::mutex> lock(m_mutex);
	auto found = m_added_trackers.find(id);
	return found != m_added_trackers.end() && found->second.count(url) > 0;
}

void Session::drop_added_trackers(std::string const& id, lt::add_torrent_params& params) const {
	std::lock_guard<std::mutex> lock(m_mutex);
	auto found = m_added_trackers.find(id);
	if (found == m_added_trackers.end() || found->second.empty()) return;
	// torrent::write_resume_data 给每个 tracker 都写了层号，与 trackers 一一对应
	std::vector<std::string> trackers;
	std::vector<int> tiers;
	for (std::size_t i = 0; i < params.trackers.size(); ++i) {
		if (found->second.count(params.trackers[i]) > 0) continue;
		trackers.push_back(params.trackers[i]);
		tiers.push_back(params.tracker_tiers.at(i));
	}
	params.trackers = std::move(trackers);
	params.tracker_tiers = std::move(tiers);
}

void Session::set_proxy(SocksProxy const& proxy) const {
	lt::settings_pack pack;
	set_proxy_of(pack, proxy);
	m_session.apply_settings(std::move(pack));
	std::lock_guard<std::mutex> net(m_network_mutex);
	m_network.proxy_error.clear();
}


void Session::reconnect_proxy(SocksProxy const& proxy) const {
	// 只有代理的设置变了 libtorrent 才重连（设成同样的值不重连），而握手、认证被拒后它不再自己重试：先换成另一个端口
	// 再换回。代理类型不变，其间不会直连；两次之间发起的节点连接与 HTTP tracker 请求连到那个端口，失败后照常重试
	lt::settings_pack detour;
	detour.set_int(lt::settings_pack::proxy_port, proxy.port == 1 ? 2 : 1);
	m_session.apply_settings(std::move(detour));
	set_proxy(proxy);
}

void Session::set_upload_limit(std::int32_t bytes_per_second) const {
	lt::settings_pack pack;
	pack.set_int(lt::settings_pack::upload_rate_limit, bytes_per_second);
	m_session.apply_settings(std::move(pack));
}

rust::Vec<Alert> Session::pop_alerts(std::uint32_t wait_ms) const {
	rust::Vec<Alert> out;
	std::lock_guard<std::mutex> lock(m_alerts_mutex);
	m_session.wait_for_alert(std::chrono::milliseconds(wait_ms));
	std::vector<lt::alert*> alerts;
	m_session.pop_alerts(&alerts);
	for (lt::alert* a : alerts) {
		// 只转现在登记的那一份的事件：事件丢了之后，移除完的种子可能在它的移除事件取走之前就算移除完、被再加入
		// （见 still_removing），那时取到的旧事件不该转给新加入的。加入失败的事件没有句柄，不查
		auto const* about = dynamic_cast<lt::torrent_alert const*>(a);
		bool const failed_add = lt::alert_cast<lt::add_torrent_alert>(a) && lt::alert_cast<lt::add_torrent_alert>(a)->error;
		if (about && !failed_add && !current(id_of(about->handle), about->handle)) continue;
		if (auto* added = lt::alert_cast<lt::add_torrent_alert>(a)) {
			if (added->error) {
				out.push_back(alert_of(AlertKind::AddFailed, *a, std::string()));
			} else {
				out.push_back(alert_of(AlertKind::Added, *a, id_of(added->handle)));
			}
		} else if (auto* t = lt::alert_cast<lt::metadata_received_alert>(a)) {
			out.push_back(alert_of(AlertKind::Metadata, *a, id_of(t->handle)));
		} else if (auto* t = lt::alert_cast<lt::torrent_finished_alert>(a)) {
			out.push_back(alert_of(AlertKind::Finished, *a, id_of(t->handle)));
		} else if (auto* t = lt::alert_cast<lt::save_resume_data_alert>(a)) {
			std::string const id = id_of(t->handle);
			drop_added_trackers(id, t->params);
			Alert alert = alert_of(AlertKind::ResumeData, *a, id);
			alert.data = bytes_out(lt::write_resume_data_buf(t->params));
			out.push_back(std::move(alert));
		} else if (auto* t = lt::alert_cast<lt::save_resume_data_failed_alert>(a)) {
			out.push_back(alert_of(AlertKind::ResumeDataFailed, *a, id_of(t->handle)));
		} else if (auto* t = lt::alert_cast<lt::cache_flushed_alert>(a)) {
			// libtorrent 在种子下完时也自己释放文件、发这个事件：只转出 flush 要的
			std::string const id = id_of(t->handle);
			if (flush_requested(id)) out.push_back(alert_of(AlertKind::CacheFlushed, *a, id));
		} else if (auto* t = lt::alert_cast<lt::fastresume_rejected_alert>(a)) {
			out.push_back(alert_of(AlertKind::ResumeRejected, *a, id_of(t->handle)));
		} else if (auto* t = lt::alert_cast<lt::torrent_error_alert>(a)) {
			// 操作系统报的错（读写、建目录、校验时读文件）即文件出错，其余为种子本身的（种子信息不对、内存不够等）
			bool const os = t->error.category() == boost::system::system_category()
				|| t->error.category() == boost::system::generic_category();
			out.push_back(alert_of(os ? AlertKind::FileError : AlertKind::TorrentError, *a, id_of(t->handle)));
		} else if (auto* t = lt::alert_cast<lt::file_error_alert>(a)) {
			out.push_back(alert_of(AlertKind::FileError, *a, id_of(t->handle)));
		} else if (auto* t = lt::alert_cast<lt::torrent_removed_alert>(a)) {
			std::string const id = id_of(t->handle);
			out.push_back(alert_of(AlertKind::Removed, *a, id));
			removed(id, false);
		} else if (auto* t = lt::alert_cast<lt::torrent_deleted_alert>(a)) {
			std::string const id = id_of(t->handle);
			out.push_back(alert_of(AlertKind::Deleted, *a, id));
			removed(id, true);
		} else if (auto* t = lt::alert_cast<lt::torrent_delete_failed_alert>(a)) {
			std::string const id = id_of(t->handle);
			out.push_back(alert_of(AlertKind::DeleteFailed, *a, id));
			removed(id, true);
		} else if (auto* t = lt::alert_cast<lt::tracker_error_alert>(a)) {
			// 跳过（announce_skipped）不是 tracker 出错：这个监听地址路由不到 tracker，如回环地址、只有 IPv4 的机器上
			// 的 IPv6 地址，别的监听地址照常通告；每个地址都被跳过的由 trackers 报
			if (t->error == lt::errors::announce_skipped) continue;
			std::string const id = id_of(t->handle);
			bool const added = added_tracker(id, t->tracker_url());
			out.push_back(alert_of(added ? AlertKind::AddedTrackerError : AlertKind::TrackerError, *a, id));
		} else if (lt::alert_cast<lt::alerts_dropped_alert>(a)) {
			mark_lost();
			out.push_back(alert_of(AlertKind::AlertsDropped, *a, std::string()));
		} else if (lt::alert_cast<lt::listen_succeeded_alert>(a)) {
			out.push_back(alert_of(AlertKind::ListenSucceeded, *a, std::string()));
		} else if (lt::alert_cast<lt::socks5_alert>(a)) {
			// 经代理的 UDP（DHT、uTP、UDP 的 tracker）连代理或握手失败：连不上、读写出错时 libtorrent 按退避自己重试，
			// 握手被拒（版本不对、要认证、认证方式不支持）后不再重试
			std::lock_guard<std::mutex> net(m_network_mutex);
			m_network.proxy_error = a->message();
		} else if (lt::alert_cast<lt::listen_failed_alert>(a)) {
			{
				std::lock_guard<std::mutex> net(m_network_mutex);
				m_network.listen_error = a->message();
			}
			out.push_back(alert_of(AlertKind::ListenFailed, *a, std::string()));
		} else if (auto* t = lt::alert_cast<lt::portmap_alert>(a)) {
			{
				std::lock_guard<std::mutex> net(m_network_mutex);
				auto const port = static_cast<std::uint16_t>(t->external_port);
				if (t->map_transport == lt::portmap_transport::upnp) {
					m_network.upnp_port = port;
					m_network.upnp_error.clear();
				} else {
					m_network.natpmp_port = port;
					m_network.natpmp_error.clear();
				}
			}
			out.push_back(alert_of(AlertKind::PortMapped, *a, std::string()));
		} else if (auto* t = lt::alert_cast<lt::portmap_error_alert>(a)) {
			{
				std::lock_guard<std::mutex> net(m_network_mutex);
				// 映射失效（租期到了没续上、路由器重启）也报这个：之前映射到的端口不再算
				if (t->map_transport == lt::portmap_transport::upnp) {
					m_network.upnp_port = 0;
					m_network.upnp_error = t->error.message();
				} else {
					m_network.natpmp_port = 0;
					m_network.natpmp_error = t->error.message();
				}
			}
			out.push_back(alert_of(AlertKind::PortMapFailed, *a, std::string()));
		} else if (auto* t = lt::alert_cast<lt::session_stats_alert>(a)) {
			static int const dht_nodes = lt::find_metric_idx("dht.dht_nodes");
			static int const incoming = lt::find_metric_idx("net.has_incoming_connections");
			auto const counters = t->counters();
			{
				std::lock_guard<std::mutex> net(m_network_mutex);
				m_network.dht_nodes = counters[dht_nodes];
				m_network.incoming = counters[incoming] != 0;
			}
			out.push_back(alert_of(AlertKind::Stats, *a, std::string()));
		}
	}
	return out;
}

rust::Vec<std::uint8_t> create_torrent(rust::Str path, CreateOptions const& options) {
	std::filesystem::path const full = std::filesystem::absolute(string_of(path));
	lt::create_torrent torrent(lt::list_files(full.string()), options.piece_size,
		options.v1_only ? lt::create_torrent::v1_only : lt::create_flags_t{});
	torrent.set_priv(options.private_torrent);
	if (!options.tracker.empty()) torrent.add_tracker(string_of(options.tracker));
	lt::set_piece_hashes(torrent, full.parent_path().string());
	return bytes_out(torrent.generate_buf());
}

rust::Vec<std::uint8_t> resume_from_torrent_file(rust::Slice<std::uint8_t const> torrent) {
	return bytes_out(lt::write_resume_data_buf(lt::load_torrent_buffer(bytes_of(torrent))));
}

namespace {

// 续传数据读出的加入参数，须有种子信息
lt::add_torrent_params with_metadata(rust::Slice<std::uint8_t const> resume) {
	lt::add_torrent_params params = lt::read_resume_data(bytes_of(resume));
	if (!params.ti) throw std::invalid_argument("续传数据里没有种子信息");
	return params;
}

} // namespace

Described describe(rust::Slice<std::uint8_t const> resume) {
	lt::add_torrent_params const params = with_metadata(resume);
	lt::file_storage const& layout = params.ti->layout();
	// 没记文件优先级时都要下
	auto const wanted = [&](lt::file_index_t const i) {
		return !layout.pad_file_at(i)
			&& (std::size_t(static_cast<int>(i)) >= params.file_priorities.size()
				|| params.file_priorities[std::size_t(static_cast<int>(i))] != lt::dont_download);
	};
	Described out;
	out.id = rust::String(canonical(params.ti->info_hashes()));
	out.name = text_of(layout.name());
	out.private_torrent = params.ti->priv();
	// 与 libtorrent 的存储起的名字相同（mmap_storage.cpp、pread_storage.cpp：按 info_hashes().get_best()）
	out.part_file = rust::String("." + lt::aux::to_hex(params.ti->info_hashes().get_best()) + ".parts");
	for (lt::file_index_t const i : layout.file_range()) {
		// 对齐用的填充文件不是内容，不下载、不列出
		if (layout.pad_file_at(i)) continue;
		auto const renamed = params.renamed_files.find(i);
		FileEntry entry;
		entry.index = static_cast<std::int32_t>(i);
		entry.path = text_of(renamed == params.renamed_files.end() ? layout.file_path(i) : renamed->second);
		entry.size = layout.file_size(i);
		entry.wanted = wanted(i);
		out.files.push_back(std::move(entry));
	}
	out.done = 0;
	for (lt::piece_index_t const piece : layout.piece_range()) {
		if (piece >= params.have_pieces.end_index() || !params.have_pieces[piece]) continue;
		for (lt::file_slice const& slice : layout.map_block(piece, 0, layout.piece_size(piece))) {
			if (wanted(slice.file_index)) out.done += slice.size;
		}
	}
	return out;
}

rust::Vec<std::uint8_t> place(rust::Slice<std::uint8_t const> resume, rust::Str root, rust::Slice<std::int32_t const> wanted) {
	lt::add_torrent_params params = with_metadata(resume);
	std::string const name = string_of(root);
	if (name.empty() || name == "." || name == ".." || name.find_first_of("/\\") != std::string::npos)
		throw std::invalid_argument("根名要是一个路径段：" + name);
	lt::file_storage const& layout = params.ti->layout();
	std::string const& original = layout.name();
	params.renamed_files.clear();
	if (name != original) {
		for (lt::file_index_t const i : layout.file_range()) {
			// 填充文件不写到磁盘，种子里没写路径的放在 .pad/ 下、不在种子名之下（torrent_info.cpp），不改
			if (layout.pad_file_at(i)) continue;
			// 单文件的种子路径即种子名，多文件的以种子名的目录开头（分隔符为本平台的）
			std::string const path = layout.file_path(i);
			if (path == original) {
				params.renamed_files[i] = name;
			} else if (path.size() > original.size() && path.compare(0, original.size(), original) == 0
				&& (path[original.size()] == '/' || path[original.size()] == '\\')) {
				params.renamed_files[i] = name + path.substr(original.size());
			} else {
				throw std::logic_error("种子里的文件不在种子名之下：" + path);
			}
		}
	}
	std::vector<lt::download_priority_t> priorities(std::size_t(layout.num_files()), lt::dont_download);
	for (std::int32_t index : wanted) {
		if (index < 0 || index >= layout.num_files() || layout.pad_file_at(lt::file_index_t(index)))
			throw std::invalid_argument("没有这个文件：" + std::to_string(index));
		priorities[std::size_t(index)] = lt::default_priority;
	}
	params.file_priorities = std::move(priorities);
	params.flags &= ~(lt::torrent_flags::upload_mode | lt::torrent_flags::paused | lt::torrent_flags::seed_mode
		| lt::torrent_flags::auto_managed);
	return bytes_out(lt::write_resume_data_buf(params));
}

} // namespace hsdlx
