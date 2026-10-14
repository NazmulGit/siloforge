<?php
/**
 * Plugin Name:       SiloForge Connect
 * Description:       Thin client connecting WordPress to a self-hosted SiloForge 2.x internal-linking backend.
 * Version:           0.2.0
 * Author:            SiloForge
 * License:           GPLv2 or later
 * License URI:       https://www.gnu.org/licenses/gpl-2.0.html
 * Text Domain:       siloforge-connect
 * Requires PHP:      7.4
 * Requires at least: 5.8
 */

if ( ! defined( 'ABSPATH' ) ) {
	exit;
}

define( 'SFC_VERSION', '0.2.0' );
define( 'SFC_PLUGIN_DIR', plugin_dir_path( __FILE__ ) );
define( 'SFC_DEFAULT_API_URL', 'http://localhost:8600' );
define( 'SFC_JWT_TRANSIENT', 'sfc_jwt' );
define( 'SFC_JWT_TTL', 25 * DAY_IN_SECONDS );
define( 'SFC_HEALTH_TRANSIENT', 'sfc_health' );
define( 'SFC_MIN_BACKEND_MAJOR', 2 );

require_once SFC_PLUGIN_DIR . 'includes/class-sfc-client.php';
require_once SFC_PLUGIN_DIR . 'includes/class-sfc-sync.php';
require_once SFC_PLUGIN_DIR . 'includes/class-sfc-linker.php';
require_once SFC_PLUGIN_DIR . 'includes/class-sfc-admin.php';
require_once SFC_PLUGIN_DIR . 'includes/class-sfc-rest.php';

/**
 * Define runtime constants from stored options.
 */
function sfc_define_runtime_constants() {
	if ( ! defined( 'SFC_API_URL' ) ) {
		$url = (string) get_option( 'siloforge_api_url', SFC_DEFAULT_API_URL );
		if ( '' === trim( $url ) ) {
			$url = SFC_DEFAULT_API_URL;
		}
		define( 'SFC_API_URL', untrailingslashit( trim( $url ) ) );
	}
	if ( ! defined( 'SFC_API_KEY' ) ) {
		define( 'SFC_API_KEY', (string) get_option( 'siloforge_api_key', '' ) );
	}
}

/**
 * Boot the plugin.
 */
function sfc_boot() {
	sfc_define_runtime_constants();

	$client = new SFC_Client();
	$sync   = new SFC_Sync( $client );
	$linker = new SFC_Linker( $client );

	$rest = new SFC_Rest( $client );
	$rest->init();

	if ( is_admin() ) {
		$admin = new SFC_Admin( $client, $sync, $linker );
		$admin->init();
	}
}
add_action( 'plugins_loaded', 'sfc_boot' );

/**
 * Load the plugin text domain.
 */
function sfc_load_textdomain() {
	load_plugin_textdomain( 'siloforge-connect', false, dirname( plugin_basename( __FILE__ ) ) . '/languages' );
}
add_action( 'init', 'sfc_load_textdomain' );
