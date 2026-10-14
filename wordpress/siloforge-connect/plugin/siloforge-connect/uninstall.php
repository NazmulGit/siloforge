<?php
/**
 * Uninstall cleanup for SiloForge Connect.
 *
 * @package siloforge-connect
 */

if ( ! defined( 'WP_UNINSTALL_PLUGIN' ) ) {
	exit;
}

delete_option( 'siloforge_api_url' );
delete_option( 'siloforge_api_key' );
delete_option( 'sfc_site_id' );
delete_option( 'sfc_last_sync' );
delete_option( 'sfc_auth_state' );
delete_option( 'sfc_pending_acks' );

delete_transient( 'sfc_jwt' );
delete_transient( 'sfc_health' );

// Remove all post meta created by the linker: content snapshots
// (_sfc_snapshot_*) and their applied-edge records (_sfc_applied_*).
global $wpdb;
$wpdb->query( "DELETE FROM {$wpdb->postmeta} WHERE meta_key LIKE '\_sfc\_%'" ); // phpcs:ignore WordPress.DB.DirectDatabaseQuery
