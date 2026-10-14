<?php
// Behaviour tests for SFC_Linker / SFC_Client with stubbed WordPress functions (no WP, no network).
// Run: docker run --rm -v "$PWD/wordpress/siloforge-connect:/w" php:7.4-cli php /w/tests/linker-test.php
define( 'ABSPATH', '/' );
define( 'MINUTE_IN_SECONDS', 60 );
define( 'DAY_IN_SECONDS', 86400 );
define( 'SFC_DEFAULT_API_URL', 'http://sf.test:8600' );
define( 'SFC_JWT_TRANSIENT', 'sfc_jwt' );
define( 'SFC_JWT_TTL', 25 * DAY_IN_SECONDS );
define( 'SFC_HEALTH_TRANSIENT', 'sfc_health' );
define( 'SFC_MIN_BACKEND_MAJOR', 2 );
define( 'SFC_VERSION', '0.2.0' );

class WP_Error {
	private $c; private $m;
	public function __construct( $c = '', $m = '' ) { $this->c = $c; $this->m = $m; }
	public function get_error_code() { return $this->c; }
	public function get_error_message() { return $this->m; }
}
$GLOBALS['opts'] = array(); $GLOBALS['tr'] = array(); $GLOBALS['meta'] = array(); $GLOBALS['posts'] = array();
$GLOBALS['http'] = array(); $GLOBALS['calls'] = array(); $GLOBALS['update_fail'] = false; $GLOBALS['ack_down'] = false;
function __( $s ) { return $s; }
function is_wp_error( $x ) { return $x instanceof WP_Error; }
function get_option( $k, $d = false ) { return array_key_exists( $k, $GLOBALS['opts'] ) ? $GLOBALS['opts'][ $k ] : $d; }
function update_option( $k, $v, $a = null ) { $GLOBALS['opts'][ $k ] = $v; return true; }
function delete_option( $k ) { unset( $GLOBALS['opts'][ $k ] ); return true; }
function get_transient( $k ) { return isset( $GLOBALS['tr'][ $k ] ) ? $GLOBALS['tr'][ $k ] : false; }
function set_transient( $k, $v, $t = 0 ) { $GLOBALS['tr'][ $k ] = $v; return true; }
function delete_transient( $k ) { unset( $GLOBALS['tr'][ $k ] ); return true; }
function untrailingslashit( $s ) { return rtrim( $s, '/\\' ); }
function wp_json_encode( $v ) { return json_encode( $v ); }
function wp_slash( $v ) { return $v; }
function esc_url( $u ) { return $u; }
function esc_url_raw( $u ) { return $u; }
function apply_filters( $h, $v ) { return $v; }
function get_post( $id ) { return isset( $GLOBALS['posts'][ $id ] ) ? $GLOBALS['posts'][ $id ] : null; }
function wp_update_post( $a, $e = false ) {
	if ( $GLOBALS['update_fail'] ) { return new WP_Error( 'db', 'db down' ); }
	$GLOBALS['posts'][ $a['ID'] ]->post_content = $a['post_content'];
	$GLOBALS['calls'][] = 'update:' . $a['ID'];
	return $a['ID'];
}
function get_post_meta( $id, $k = '', $s = false ) {
	$m = isset( $GLOBALS['meta'][ $id ] ) ? $GLOBALS['meta'][ $id ] : array();
	if ( '' === $k ) { $o = array(); foreach ( $m as $kk => $vv ) { $o[ $kk ] = array( $vv ); } return $o; }
	return isset( $m[ $k ] ) ? $m[ $k ] : '';
}
function add_post_meta( $id, $k, $v ) { $GLOBALS['meta'][ $id ][ $k ] = $v; return true; }
function update_post_meta( $id, $k, $v ) { $GLOBALS['meta'][ $id ][ $k ] = $v; return true; }
function delete_post_meta( $id, $k ) { unset( $GLOBALS['meta'][ $id ][ $k ] ); return true; }
function wp_remote_retrieve_response_code( $r ) { return $r['code']; }
function wp_remote_retrieve_body( $r ) { return $r['body']; }
function fake_http( $method, $url, $args ) {
	$path = (string) parse_url( $url, PHP_URL_PATH );
	$body = isset( $args['body'] ) ? json_decode( $args['body'], true ) : null;
	$GLOBALS['calls'][] = $method . ' ' . $path . ( $body ? ' ' . json_encode( $body ) : '' );
	if ( '/health' === $path ) { return array( 'code' => 200, 'body' => '{"ok":true,"version":"2.1.0"}' ); }
	if ( '/api/v1/auth/handshake' === $path ) {
		return $body['api_key'] === $GLOBALS['valid_key'] ? array( 'code' => 200, 'body' => '{"token":"jwt-' . $GLOBALS['valid_key'] . '","site_id":"11111111-1111-1111-1111-111111111111"}' ) : array( 'code' => 401, 'body' => '{"error":"invalid api key"}' );
	}
	$auth = isset( $args['headers']['Authorization'] ) ? $args['headers']['Authorization'] : '';
	if ( $auth !== 'Bearer jwt-' . $GLOBALS['valid_key'] ) { return array( 'code' => 401, 'body' => '{"error":"unauthorized"}' ); }
	if ( preg_match( '#/ack$#', $path ) && $GLOBALS['ack_down'] ) { return new WP_Error( 'http_request_failed', 'connection refused' ); }
	if ( preg_match( '#^/api/v1/suggestions/[^/]+$#', $path ) ) { return array( 'code' => 200, 'body' => json_encode( $GLOBALS['suggestions'] ) ); }
	return array( 'code' => 200, 'body' => '{}' );
}
function wp_remote_get( $u, $a = array() ) { return fake_http( 'GET', $u, $a ); }
function wp_remote_post( $u, $a = array() ) { return fake_http( isset( $a['method'] ) ? $a['method'] : 'POST', $u, $a ); }

require __DIR__ . '/../plugin/siloforge-connect/includes/class-sfc-client.php';
require __DIR__ . '/../plugin/siloforge-connect/includes/class-sfc-linker.php';

$fails = 0;
function check( $name, $cond ) { global $fails; echo ( $cond ? 'ok   ' : 'FAIL ' ), $name, "\n"; if ( ! $cond ) { $fails++; } }
function reset_state( $content ) {
	$GLOBALS['opts'] = array( 'siloforge_api_url' => 'http://sf.test:8600', 'siloforge_api_key' => 'sf_good', 'sfc_site_id' => '11111111-1111-1111-1111-111111111111' );
	$GLOBALS['tr'] = array(); $GLOBALS['meta'] = array(); $GLOBALS['calls'] = array();
	$GLOBALS['update_fail'] = false; $GLOBALS['ack_down'] = false; $GLOBALS['valid_key'] = 'sf_good';
	$GLOBALS['posts'] = array( 7 => (object) array( 'ID' => 7, 'post_content' => $content ) );
}
function acks() { return array_values( array_filter( $GLOBALS['calls'], function ( $c ) { return false !== strpos( $c, '/ack' ); } ) ); }
function sugg( $edge, $anchor, $hash, $idx = 1 ) {
	return array( 'edge_id' => $edge, 'wp_post_id' => 7, 'target_url' => 'https://site.test/target/', 'anchor' => $anchor, 'paragraph_index' => $idx, 'paragraph_hash' => $hash, 'replacement_html' => null, 'insert_module' => 'native' );
}

$p0 = '<p>Short intro.</p>';
$p1 = '<p>Choosing the right <strong>bike lock</strong> keeps your bicycle safe on busy city streets every day.</p>';
$p2 = '<p>A good bike lock should be heavy, but a bike lock that is too heavy stays at home.</p>';
$doc = $p0 . "\n" . $p1 . "\n" . $p2;
$h1 = sha1( 'Choosing the right bike lock keeps your bicycle safe on busy city streets every day.' );
$h2 = sha1( 'A good bike lock should be heavy, but a bike lock that is too heavy stays at home.' );
$e1 = 'aaaaaaaa-0000-0000-0000-000000000001'; $e2 = 'aaaaaaaa-0000-0000-0000-000000000002'; $e3 = 'aaaaaaaa-0000-0000-0000-000000000003';

// 1. Hash verified: link goes into the hashed paragraph (p2), not the first occurrence (p1); ack after update.
reset_state( $doc );
$GLOBALS['suggestions'] = array( sugg( $e1, 'bike lock', $h2, 1 ) );
$linker = new SFC_Linker( new SFC_Client() );
$r = $linker->pull_and_apply();
$c = $GLOBALS['posts'][7]->post_content;
check( 'hash: applied 1', 1 === $r['applied'] && 'hash' === $r['details'][0]['verified'] );
check( 'hash: link in paragraph 2', false !== strpos( $c, 'A good <a href="https://site.test/target/">bike lock</a> should' ) && false === strpos( substr( $c, 0, strlen( $p0 ) + strlen( $p1 ) + 1 ), '<a ' ) );
$iu = array_search( 'update:7', $GLOBALS['calls'], true ); $ia = null;
foreach ( $GLOBALS['calls'] as $i => $call ) { if ( false !== strpos( $call, '/ack' ) ) { $ia = $i; } }
check( 'hash: ack ok after wp_update_post', false !== $iu && null !== $ia && $ia > $iu && false !== strpos( acks()[0], '"ok":true' ) );
check( 'hash: snapshot + applied meta', 2 === count( $GLOBALS['meta'][7] ) );

// 2. Hash mismatch and anchor not in indexed paragraph -> skipped, ack content changed, no update.
reset_state( $doc );
$GLOBALS['suggestions'] = array( sugg( $e1, 'city streets', sha1( 'something else entirely, long enough to be a paragraph' ), 1 ) );
$GLOBALS['suggestions'][0]['paragraph_index'] = 9;
$r = ( new SFC_Linker( new SFC_Client() ) )->pull_and_apply();
check( 'mismatch: skipped', 1 === $r['skipped'] && 'content changed' === $r['details'][0]['reason'] );
check( 'mismatch: ack content changed', 1 === count( acks() ) && false !== strpos( acks()[0], '"ok":false,"error":"content changed"' ) );
check( 'mismatch: no update', ! in_array( 'update:7', $GLOBALS['calls'], true ) && $GLOBALS['posts'][7]->post_content === $doc );

// 3. Hash mismatch but anchor present in the indexed paragraph -> applied (verified by anchor).
reset_state( $doc );
$GLOBALS['suggestions'] = array( sugg( $e1, 'city streets', sha1( 'stale text from an older chunking of this post, long' ), 0 ) );
$r = ( new SFC_Linker( new SFC_Client() ) )->pull_and_apply();
check( 'anchor fallback: applied', 1 === $r['applied'] && 'anchor' === $r['details'][0]['verified'] && false !== strpos( $GLOBALS['posts'][7]->post_content, '>city streets</a>' ) );

// 4. wp_update_post fails -> acks ok:false, snapshot removed (R8).
reset_state( $doc );
$GLOBALS['update_fail'] = true;
$GLOBALS['suggestions'] = array( sugg( $e1, 'bike lock', $h2 ), sugg( $e2, 'city streets', $h1 ) );
$r = ( new SFC_Linker( new SFC_Client() ) )->pull_and_apply();
$a = acks();
check( 'update fail: 2 failed, 0 applied', 2 === $r['failed'] && 0 === $r['applied'] );
check( 'update fail: acks ok:false', 2 === count( $a ) && false !== strpos( $a[0], '"ok":false' ) && false !== strpos( $a[1], 'wp_update_post failed' ) );
check( 'update fail: no snapshot left', empty( $GLOBALS['meta'][7] ) );

// 5. Two edges, one post, one save: both links, single update, two ok acks.
reset_state( $doc );
$GLOBALS['suggestions'] = array( sugg( $e1, 'bike lock', $h2 ), sugg( $e2, 'city streets', $h1 ) );
$r = ( new SFC_Linker( new SFC_Client() ) )->pull_and_apply();
check( 'two edges: one update, two links', 1 === count( array_keys( $GLOBALS['calls'], 'update:7', true ) ) && 2 === substr_count( $GLOBALS['posts'][7]->post_content, '<a ' ) && 2 === $r['applied'] );

// 6. Ack delivery fails after save -> queued; next pull re-sends and does not re-apply.
reset_state( $doc );
$GLOBALS['ack_down'] = true;
$GLOBALS['suggestions'] = array( sugg( $e3, 'bike lock', $h2 ) );
( new SFC_Linker( new SFC_Client() ) )->pull_and_apply();
$q = get_option( 'sfc_pending_acks', array() );
check( 'ack down: queued', isset( $q[ $e3 ] ) );
$after = $GLOBALS['posts'][7]->post_content;
$r = ( new SFC_Linker( new SFC_Client() ) )->pull_and_apply();
check( 'ack down: still pending, not re-applied', 1 === $r['skipped'] && $GLOBALS['posts'][7]->post_content === $after );
$GLOBALS['ack_down'] = false; $GLOBALS['calls'] = array();
$r = ( new SFC_Linker( new SFC_Client() ) )->pull_and_apply();
check( 'ack restored: queue flushed', array() === get_option( 'sfc_pending_acks', array() ) && 1 === count( acks() ) );

// 7. Key rotation: old JWT -> 401, re-handshake with old key -> 401 -> key_rejected; no hammering.
reset_state( $doc );
$GLOBALS['tr']['sfc_jwt'] = 'jwt-sf_good';
$GLOBALS['valid_key'] = 'sf_rotated';
$client = new SFC_Client();
$res = $client->authed_request( 'GET', '/api/v1/suggestions/x' );
check( 'rotation: key_rejected error', is_wp_error( $res ) && 'sfc_key_rejected' === $res->get_error_code() && 'key_rejected' === $client->connection_status() );
$GLOBALS['calls'] = array();
$res = $client->authed_request( 'GET', '/api/v1/suggestions/x' );
check( 'rotation: no retry while rejected', is_wp_error( $res ) && array() === $GLOBALS['calls'] );
// New key saved (the admin sanitize callback deletes sfc_auth_state).
$GLOBALS['opts']['siloforge_api_key'] = 'sf_rotated'; unset( $GLOBALS['opts']['sfc_auth_state'] );
$client2 = new SFC_Client();
$res = $client2->authed_request( 'GET', '/api/v1/suggestions/x' );
check( 'rotation: new key reconnects', ! is_wp_error( $res ) && 'connected' === $client2->connection_status() );

// 8. Health + version gate.
$h = $client2->health( true );
check( 'health: version 2.1.0', ! is_wp_error( $h ) && '2.1.0' === $h['version'] && SFC_Client::backend_supported( '2.1.0' ) && ! SFC_Client::backend_supported( '0.9.3' ) );

// 9. No paragraph_hash (legacy edge): first safe occurrence.
reset_state( $doc );
$GLOBALS['suggestions'] = array( sugg( $e1, 'bike lock', null ) );
$r = ( new SFC_Linker( new SFC_Client() ) )->pull_and_apply();
check( 'legacy: applied, first safe occurrence (p2, p1 has it inside <strong>)', 1 === $r['applied'] );

// 10. Classic content without <p>: blank-line blocks hash like wpautop paragraphs.
$classic = "Intro line.\n\nChoosing the right bike lock keeps your bicycle safe on busy city streets every day.\n\nEnd.";
reset_state( $classic );
$GLOBALS['suggestions'] = array( sugg( $e1, 'bicycle', $h1 ) );
$r = ( new SFC_Linker( new SFC_Client() ) )->pull_and_apply();
check( 'classic: hash on blank-line block', 1 === $r['applied'] && 'hash' === $r['details'][0]['verified'] && false !== strpos( $GLOBALS['posts'][7]->post_content, 'your <a href="https://site.test/target/">bicycle</a> safe' ) );

// 11. Hostile input never fatals.
foreach ( array( '', '<p', str_repeat( '<div>', 3000 ), "\xff\xfe<p>bad utf8 \xc3\x28 text that is long enough for a paragraph</p>", '<p>' . str_repeat( 'x', 200000 ) . '</p>' ) as $hostile ) {
	SFC_Linker::candidate_blocks( $hostile );
}
check( 'hostile input: no fatal', true );

echo $fails ? "FAILURES: $fails\n" : "ALL PASS\n";
exit( $fails ? 1 : 0 );
