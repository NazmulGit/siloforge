<?php
/**
 * REST API: siloforge/v1 namespace.
 *
 * @package siloforge-connect
 */

if ( ! defined( 'ABSPATH' ) ) {
	exit;
}

/**
 * Registers GET /wp-json/siloforge/v1/status for the admin UI.
 */
class SFC_Rest {

	/**
	 * API client.
	 *
	 * @var SFC_Client
	 */
	private $client;

	/**
	 * Constructor.
	 *
	 * @param SFC_Client $client API client.
	 */
	public function __construct( SFC_Client $client ) {
		$this->client = $client;
	}

	/**
	 * Hook route registration.
	 *
	 * @return void
	 */
	public function init() {
		add_action( 'rest_api_init', array( $this, 'register_routes' ) );
	}

	/**
	 * Register routes.
	 *
	 * @return void
	 */
	public function register_routes() {
		register_rest_route(
			'siloforge/v1',
			'/status',
			array(
				'methods'             => WP_REST_Server::READABLE,
				'callback'            => array( $this, 'status' ),
				'permission_callback' => static function () {
					return current_user_can( 'manage_options' );
				},
			)
		);
	}

	/**
	 * Status payload.
	 *
	 * @return WP_REST_Response
	 */
	public function status() {
		return rest_ensure_response(
			array(
				'connected'      => $this->client->is_connected(),
				'site_id'        => $this->client->get_site_id(),
				'last_sync'      => get_option( 'sfc_last_sync', null ),
				// Additive (0.2.0).
				'plugin_version' => SFC_VERSION,
				'status'         => $this->client->connection_status(),
			)
		);
	}
}
