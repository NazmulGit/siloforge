<?php
/**
 * HTTP client for the SILOFORGE backend API.
 *
 * @package siloforge-connect
 */

if ( ! defined( 'ABSPATH' ) ) {
	exit;
}

/**
 * Thin wrapper around wp_remote_get()/wp_remote_post() handling
 * the site-token handshake and Bearer-authenticated requests.
 */
class SFC_Client {

	/**
	 * Base API URL, no trailing slash.
	 *
	 * @return string
	 */
	public function api_url() {
		$url = defined( 'SFC_API_URL' ) ? SFC_API_URL : (string) get_option( 'siloforge_api_url', SFC_DEFAULT_API_URL );
		if ( '' === trim( $url ) ) {
			$url = SFC_DEFAULT_API_URL;
		}
		return untrailingslashit( trim( $url ) );
	}

	/**
	 * Configured API key.
	 *
	 * @return string
	 */
	public function api_key() {
		$key = defined( 'SFC_API_KEY' ) ? SFC_API_KEY : (string) get_option( 'siloforge_api_key', '' );
		return trim( $key );
	}

	/**
	 * Site id received from the last successful handshake.
	 *
	 * @return string
	 */
	public function get_site_id() {
		return (string) get_option( 'sfc_site_id', '' );
	}

	/**
	 * Whether we hold a cached JWT and a known site id.
	 *
	 * @return bool
	 */
	public function is_connected() {
		return ( '' !== $this->get_site_id() && false !== get_transient( SFC_JWT_TRANSIENT ) );
	}

	/**
	 * POST /api/v1/auth/handshake with the API key; caches the JWT
	 * in the 'sfc_jwt' transient (25 days) and stores the site id.
	 *
	 * @param string|null $api_key Optional key override; defaults to the stored option.
	 * @return array|WP_Error Array with 'token' and 'site_id' on success.
	 */
	public function handshake( $api_key = null ) {
		if ( null === $api_key ) {
			$api_key = $this->api_key();
		}
		$api_key = trim( (string) $api_key );
		if ( '' === $api_key ) {
			return new WP_Error( 'sfc_no_key', __( 'No SiloForge API key is configured.', 'siloforge-connect' ) );
		}

		$response = wp_remote_post(
			$this->api_url() . '/api/v1/auth/handshake',
			array(
				'timeout' => 30,
				'headers' => array( 'Content-Type' => 'application/json' ),
				'body'    => wp_json_encode( array( 'api_key' => $api_key ) ),
			)
		);
		if ( is_wp_error( $response ) ) {
			return $response;
		}

		$code = (int) wp_remote_retrieve_response_code( $response );
		$data = json_decode( wp_remote_retrieve_body( $response ), true );
		if ( 401 === $code ) {
			// The backend does not know this key: a wrong key, or it was rotated in SiloForge
			// (Sites -> Rotate key). Remember that, so background requests stop retrying a dead
			// key (the backend rate-limits failed handshakes) until the key changes.
			delete_transient( SFC_JWT_TRANSIENT );
			$this->set_auth_state( 'key_rejected', $api_key );
			return new WP_Error(
				'sfc_key_rejected',
				__( 'SiloForge rejected the API key (HTTP 401). If the site key was rotated in SiloForge, paste the new key here and save.', 'siloforge-connect' )
			);
		}
		if ( 429 === $code ) {
			return new WP_Error(
				'sfc_rate_limited',
				__( 'SiloForge is rate-limiting handshakes after repeated failures (HTTP 429). Wait a few minutes and try again.', 'siloforge-connect' )
			);
		}
		if ( 200 !== $code || ! is_array( $data ) || empty( $data['token'] ) ) {
			return new WP_Error(
				'sfc_handshake_failed',
				sprintf(
					/* translators: %d: HTTP status code. */
					__( 'SiloForge handshake failed (HTTP %d). Check the API URL and API key.', 'siloforge-connect' ),
					$code
				)
			);
		}

		set_transient( SFC_JWT_TRANSIENT, (string) $data['token'], SFC_JWT_TTL );
		if ( ! empty( $data['site_id'] ) ) {
			update_option( 'sfc_site_id', (string) $data['site_id'], false );
		}
		$this->set_auth_state( 'connected', $api_key );

		return array(
			'token'   => (string) $data['token'],
			'site_id' => isset( $data['site_id'] ) ? (string) $data['site_id'] : $this->get_site_id(),
		);
	}

	/**
	 * Perform an authenticated JSON request. On a 401 the JWT is
	 * discarded, the handshake re-run once, and the request retried.
	 *
	 * @param string     $method HTTP method ('GET' or 'POST').
	 * @param string     $path   Path starting with '/', e.g. '/api/v1/sync'.
	 * @param array|null $body   Optional body, JSON-encoded when not null.
	 * @return array|WP_Error Decoded JSON response body.
	 */
	public function authed_request( $method, $path, $body = null ) {
		return $this->do_authed_request( $method, $path, $body, false );
	}

	/**
	 * Internal request runner with a single-retry guard.
	 *
	 * @param string     $method  HTTP method.
	 * @param string     $path    Request path.
	 * @param array|null $body    Optional body.
	 * @param bool       $retried Whether this is already the retry.
	 * @return array|WP_Error
	 */
	private function do_authed_request( $method, $path, $body, $retried ) {
		$token = get_transient( SFC_JWT_TRANSIENT );
		if ( ! is_string( $token ) || '' === $token ) {
			if ( $this->key_recently_rejected() ) {
				return $this->rejected_error();
			}
			$handshake = $this->handshake();
			if ( is_wp_error( $handshake ) ) {
				return $handshake;
			}
			$token = $handshake['token'];
		}

		$method = strtoupper( (string) $method );
		$args   = array(
			'timeout' => 60,
			'headers' => array(
				'Authorization' => 'Bearer ' . $token,
				'Content-Type'  => 'application/json',
			),
		);
		if ( null !== $body ) {
			$args['body'] = wp_json_encode( $body );
		}

		$url = $this->api_url() . $path;
		if ( 'GET' === $method ) {
			$response = wp_remote_get( $url, $args );
		} else {
			$args['method'] = $method;
			$response       = wp_remote_post( $url, $args );
		}
		if ( is_wp_error( $response ) ) {
			return $response;
		}

		$code = (int) wp_remote_retrieve_response_code( $response );
		if ( 401 === $code ) {
			// The JWT is no longer valid: it expired, the backend JWT secret changed, or the site
			// key was rotated (the JWT carries a fingerprint of the key). Re-handshake once; with
			// a rotated key the handshake then fails with a clear "key rejected" error.
			delete_transient( SFC_JWT_TRANSIENT );
			if ( $retried ) {
				return new WP_Error( 'sfc_http_401', __( 'SiloForge still answers 401 after a fresh handshake. Check the API key and the site in SiloForge.', 'siloforge-connect' ) );
			}
			$handshake = $this->handshake();
			if ( is_wp_error( $handshake ) ) {
				return $handshake;
			}
			return $this->do_authed_request( $method, $path, $body, true );
		}

		$data = json_decode( wp_remote_retrieve_body( $response ), true );
		if ( $code < 200 || $code >= 300 ) {
			$detail = ( is_array( $data ) && isset( $data['error'] ) && is_string( $data['error'] ) ) ? $data['error'] : '';
			return new WP_Error(
				'sfc_http_' . $code,
				sprintf(
					/* translators: 1: HTTP status code, 2: request path, 3: error detail. */
					__( 'SiloForge API error (HTTP %1$d) on %2$s. %3$s', 'siloforge-connect' ),
					$code,
					$path,
					$detail
				)
			);
		}

		return is_array( $data ) ? $data : array();
	}

	/**
	 * GET /health (no auth). Cached for a minute unless $refresh.
	 *
	 * @param bool $refresh Bypass the cache.
	 * @return array|WP_Error Array with ok, version, checked (UTC).
	 */
	public function health( $refresh = false ) {
		if ( ! $refresh ) {
			$cached = get_transient( SFC_HEALTH_TRANSIENT );
			if ( is_array( $cached ) ) {
				return $cached;
			}
		}
		$response = wp_remote_get( $this->api_url() . '/health', array( 'timeout' => 8 ) );
		if ( is_wp_error( $response ) ) {
			return $response;
		}
		$code = (int) wp_remote_retrieve_response_code( $response );
		$data = json_decode( wp_remote_retrieve_body( $response ), true );
		if ( 200 !== $code || ! is_array( $data ) ) {
			return new WP_Error(
				'sfc_health_failed',
				sprintf(
					/* translators: %d: HTTP status code. */
					__( 'SiloForge /health answered HTTP %d.', 'siloforge-connect' ),
					$code
				)
			);
		}
		$health = array(
			'ok'      => ! empty( $data['ok'] ),
			'version' => ( isset( $data['version'] ) && is_scalar( $data['version'] ) ) ? (string) $data['version'] : '',
			'checked' => gmdate( 'Y-m-d H:i:s' ),
		);
		set_transient( SFC_HEALTH_TRANSIENT, $health, MINUTE_IN_SECONDS );
		return $health;
	}

	/**
	 * Whether a backend version string is a supported major (SiloForge 2.x or later).
	 *
	 * @param string $version Backend version, e.g. "2.1.0".
	 * @return bool
	 */
	public static function backend_supported( $version ) {
		$major = (int) strtok( (string) $version, '.' );
		return $major >= SFC_MIN_BACKEND_MAJOR;
	}

	/**
	 * Last authentication state: array with state, key_fp, time (or an empty array).
	 *
	 * @return array
	 */
	public function auth_state() {
		$state = get_option( 'sfc_auth_state', array() );
		return is_array( $state ) ? $state : array();
	}

	/**
	 * Connection state for the settings page: no_key, key_rejected, connected or not_connected.
	 *
	 * @return string
	 */
	public function connection_status() {
		$key = $this->api_key();
		if ( '' === $key ) {
			return 'no_key';
		}
		$state = $this->auth_state();
		$same  = isset( $state['key_fp'] ) && self::key_fingerprint( $key ) === $state['key_fp'];
		if ( $same && isset( $state['state'] ) && 'key_rejected' === $state['state'] ) {
			return 'key_rejected';
		}
		if ( $this->is_connected() ) {
			return 'connected';
		}
		return 'not_connected';
	}

	/**
	 * Record the authentication state for a key (only a short fingerprint is stored).
	 *
	 * @param string $state   'connected' or 'key_rejected'.
	 * @param string $api_key Key used.
	 * @return void
	 */
	private function set_auth_state( $state, $api_key ) {
		update_option(
			'sfc_auth_state',
			array(
				'state'  => (string) $state,
				'key_fp' => self::key_fingerprint( $api_key ),
				'time'   => time(),
			),
			false
		);
	}

	/**
	 * Short one-way fingerprint of a key, used to notice when the key changes.
	 *
	 * @param string $api_key Key.
	 * @return string
	 */
	private static function key_fingerprint( $api_key ) {
		return substr( hash( 'sha256', 'sfc:' . trim( (string) $api_key ) ), 0, 16 );
	}

	/**
	 * True when the current key was rejected by the backend less than 10 minutes ago.
	 * Saving a different key or clicking "Test Connection" always handshakes again.
	 *
	 * @return bool
	 */
	public function key_recently_rejected() {
		$state = $this->auth_state();
		return isset( $state['state'], $state['key_fp'], $state['time'] )
			&& 'key_rejected' === $state['state']
			&& self::key_fingerprint( $this->api_key() ) === $state['key_fp']
			&& ( time() - (int) $state['time'] ) < 10 * MINUTE_IN_SECONDS;
	}

	/**
	 * Error returned while the current key is known to be rejected.
	 *
	 * @return WP_Error
	 */
	private function rejected_error() {
		return new WP_Error(
			'sfc_key_rejected',
			__( 'The SiloForge API key was rejected (it may have been rotated). Paste the new key in Settings -> SiloForge and save, or click Test Connection.', 'siloforge-connect' )
		);
	}
}
