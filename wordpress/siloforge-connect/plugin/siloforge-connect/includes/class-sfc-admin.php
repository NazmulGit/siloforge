<?php
/**
 * Admin settings page and action handlers.
 *
 * @package siloforge-connect
 */

if ( ! defined( 'ABSPATH' ) ) {
	exit;
}

/**
 * Settings page (Settings → SiloForge) plus admin-post action handlers.
 */
class SFC_Admin {

	const PAGE_SLUG = 'siloforge-connect';

	/**
	 * API client.
	 *
	 * @var SFC_Client
	 */
	private $client;

	/**
	 * Sync service.
	 *
	 * @var SFC_Sync
	 */
	private $sync;

	/**
	 * Linker service.
	 *
	 * @var SFC_Linker
	 */
	private $linker;

	/**
	 * Constructor.
	 *
	 * @param SFC_Client $client API client.
	 * @param SFC_Sync   $sync   Sync service.
	 * @param SFC_Linker $linker Linker service.
	 */
	public function __construct( SFC_Client $client, SFC_Sync $sync, SFC_Linker $linker ) {
		$this->client = $client;
		$this->sync   = $sync;
		$this->linker = $linker;
	}

	/**
	 * Hook everything up.
	 *
	 * @return void
	 */
	public function init() {
		add_action( 'admin_menu', array( $this, 'register_menu' ) );
		add_action( 'admin_init', array( $this, 'register_settings' ) );
		add_action( 'admin_post_sfc_test_connection', array( $this, 'handle_test_connection' ) );
		add_action( 'admin_post_sfc_sync_now', array( $this, 'handle_sync_now' ) );
		add_action( 'admin_post_sfc_pull_apply', array( $this, 'handle_pull_apply' ) );
		add_action( 'admin_notices', array( $this, 'render_notices' ) );
	}

	/**
	 * Register Settings → SiloForge.
	 *
	 * @return void
	 */
	public function register_menu() {
		add_options_page(
			__( 'SiloForge Connect', 'siloforge-connect' ),
			__( 'SiloForge', 'siloforge-connect' ),
			'manage_options',
			self::PAGE_SLUG,
			array( $this, 'render_page' )
		);
	}

	/**
	 * Register plugin settings.
	 *
	 * @return void
	 */
	public function register_settings() {
		register_setting(
			'sfc_settings',
			'siloforge_api_url',
			array(
				'type'              => 'string',
				'sanitize_callback' => array( $this, 'sanitize_api_url' ),
				'default'           => SFC_DEFAULT_API_URL,
			)
		);
		register_setting(
			'sfc_settings',
			'siloforge_api_key',
			array(
				'type'              => 'string',
				'sanitize_callback' => array( $this, 'sanitize_api_key' ),
				'default'           => '',
			)
		);
	}

	/**
	 * Sanitize the API URL option.
	 *
	 * @param string $value Raw value.
	 * @return string
	 */
	public function sanitize_api_url( $value ) {
		$value = esc_url_raw( trim( (string) $value ), array( 'http', 'https' ) );
		return untrailingslashit( $value );
	}

	/**
	 * Sanitize the API key; drop the cached JWT when the key changes.
	 *
	 * @param string $value Raw value.
	 * @return string
	 */
	public function sanitize_api_key( $value ) {
		$value = trim( sanitize_text_field( (string) $value ) );
		if ( $value !== get_option( 'siloforge_api_key', '' ) ) {
			// New (e.g. rotated) key: forget the old JWT and the "key rejected" state.
			delete_transient( SFC_JWT_TRANSIENT );
			delete_option( 'sfc_auth_state' );
		}
		return $value;
	}

	/**
	 * Handle "Test Connection".
	 *
	 * @return void
	 */
	public function handle_test_connection() {
		$this->guard( 'sfc_test_connection' );

		delete_transient( SFC_JWT_TRANSIENT );
		$health  = $this->client->health( true );
		$version = is_wp_error( $health ) ? '' : $health['version'];
		$result  = $this->client->handshake();
		if ( is_wp_error( $result ) ) {
			$this->set_notice( 'error', $result->get_error_message() );
		} elseif ( '' !== $version && ! SFC_Client::backend_supported( $version ) ) {
			$this->set_notice(
				'error',
				sprintf(
					/* translators: 1: backend version, 2: plugin version. */
					__( 'Connected, but the backend is SiloForge %1$s. SiloForge Connect %2$s requires SiloForge 2.x; upgrade the backend.', 'siloforge-connect' ),
					$version,
					SFC_VERSION
				)
			);
		} else {
			$this->set_notice(
				'success',
				sprintf(
					/* translators: 1: site id, 2: backend version. */
					__( 'Connected to SiloForge %2$s. Site ID: %1$s', 'siloforge-connect' ),
					$result['site_id'],
					'' !== $version ? $version : '?'
				)
			);
		}

		$this->redirect_back();
	}

	/**
	 * Handle "Sync Now".
	 *
	 * @return void
	 */
	public function handle_sync_now() {
		$this->guard( 'sfc_sync_now' );

		$result = $this->sync->sync_all();
		if ( is_wp_error( $result ) ) {
			$this->set_notice( 'error', $result->get_error_message() );
		} else {
			$this->set_notice(
				'success',
				sprintf(
					/* translators: 1: posts sent, 2: batches, 3: received, 4: upserted, 5: skipped. */
					__( 'Sync complete: %1$d posts in %2$d batches. Backend received %3$d, upserted %4$d, skipped %5$d.', 'siloforge-connect' ),
					$result['posts'],
					$result['batches'],
					$result['received'],
					$result['upserted'],
					$result['skipped']
				)
			);
		}

		$this->redirect_back();
	}

	/**
	 * Handle "Pull & Apply Links".
	 *
	 * @return void
	 */
	public function handle_pull_apply() {
		$this->guard( 'sfc_pull_apply' );

		$result = $this->linker->pull_and_apply();
		if ( is_wp_error( $result ) ) {
			$this->set_notice( 'error', $result->get_error_message() );
		} else {
			$type = ( $result['failed'] > 0 ) ? 'error' : 'success';
			$this->set_notice(
				$type,
				sprintf(
					/* translators: 1: total suggestions, 2: applied, 3: skipped, 4: failed, 5: posts updated. */
					__( 'Pull complete: %1$d suggestions — %2$d applied, %3$d skipped, %4$d failed, %5$d posts updated.', 'siloforge-connect' ),
					$result['total'],
					$result['applied'],
					$result['skipped'],
					$result['failed'],
					$result['posts_updated']
				)
			);
		}

		$this->redirect_back();
	}

	/**
	 * Show queued notices on our settings page.
	 *
	 * @return void
	 */
	public function render_notices() {
		$screen = function_exists( 'get_current_screen' ) ? get_current_screen() : null;
		if ( ! $screen || 'settings_page_' . self::PAGE_SLUG !== $screen->id ) {
			return;
		}

		$key    = 'sfc_notice_' . get_current_user_id();
		$notice = get_transient( $key );
		if ( ! is_array( $notice ) || empty( $notice['message'] ) ) {
			return;
		}
		delete_transient( $key );

		$class = ( isset( $notice['type'] ) && 'success' === $notice['type'] ) ? 'notice-success' : 'notice-error';
		printf(
			'<div class="notice %1$s is-dismissible"><p>%2$s</p></div>',
			esc_attr( $class ),
			esc_html( $notice['message'] )
		);
	}

	/**
	 * Render the settings page.
	 *
	 * @return void
	 */
	public function render_page() {
		if ( ! current_user_can( 'manage_options' ) ) {
			return;
		}

		$api_url   = (string) get_option( 'siloforge_api_url', SFC_DEFAULT_API_URL );
		$api_key   = (string) get_option( 'siloforge_api_key', '' );
		$site_id   = $this->client->get_site_id();
		$last_sync = get_option( 'sfc_last_sync', array() );
		?>
		<div class="wrap">
			<h1><?php echo esc_html__( 'SiloForge Connect', 'siloforge-connect' ); ?> <small style="font-weight:normal;">v<?php echo esc_html( SFC_VERSION ); ?></small></h1>

			<?php $this->render_connection(); ?>

			<form method="post" action="options.php">
				<?php settings_fields( 'sfc_settings' ); ?>
				<table class="form-table" role="presentation">
					<tr>
						<th scope="row">
							<label for="siloforge_api_url"><?php echo esc_html__( 'API URL', 'siloforge-connect' ); ?></label>
						</th>
						<td>
							<input type="url" class="regular-text code" id="siloforge_api_url" name="siloforge_api_url" value="<?php echo esc_attr( $api_url ); ?>" placeholder="<?php echo esc_attr( SFC_DEFAULT_API_URL ); ?>" />
							<p class="description"><?php echo esc_html__( 'Base URL of the SILOFORGE backend, e.g. http://backend-host:8600', 'siloforge-connect' ); ?></p>
						</td>
					</tr>
					<tr>
						<th scope="row">
							<label for="siloforge_api_key"><?php echo esc_html__( 'API Key', 'siloforge-connect' ); ?></label>
						</th>
						<td>
							<input type="password" class="regular-text code" id="siloforge_api_key" name="siloforge_api_key" value="<?php echo esc_attr( $api_key ); ?>" autocomplete="off" />
							<p class="description"><?php echo esc_html__( 'Site API key generated by the SILOFORGE backend (sf_...).', 'siloforge-connect' ); ?></p>
						</td>
					</tr>
					<tr>
						<th scope="row"><?php echo esc_html__( 'Site ID', 'siloforge-connect' ); ?></th>
						<td>
							<code><?php echo '' !== $site_id ? esc_html( $site_id ) : esc_html__( 'not connected yet', 'siloforge-connect' ); ?></code>
						</td>
					</tr>
				</table>
				<?php submit_button( __( 'Save Settings', 'siloforge-connect' ) ); ?>
			</form>

			<h2><?php echo esc_html__( 'Actions', 'siloforge-connect' ); ?></h2>
			<div style="display:flex;gap:8px;flex-wrap:wrap;">
				<form method="post" action="<?php echo esc_url( admin_url( 'admin-post.php' ) ); ?>">
					<input type="hidden" name="action" value="sfc_test_connection" />
					<?php wp_nonce_field( 'sfc_test_connection' ); ?>
					<?php submit_button( __( 'Test Connection', 'siloforge-connect' ), 'secondary', 'submit', false ); ?>
				</form>
				<form method="post" action="<?php echo esc_url( admin_url( 'admin-post.php' ) ); ?>">
					<input type="hidden" name="action" value="sfc_sync_now" />
					<?php wp_nonce_field( 'sfc_sync_now' ); ?>
					<?php submit_button( __( 'Sync Now', 'siloforge-connect' ), 'primary', 'submit', false ); ?>
				</form>
				<form method="post" action="<?php echo esc_url( admin_url( 'admin-post.php' ) ); ?>">
					<input type="hidden" name="action" value="sfc_pull_apply" />
					<?php wp_nonce_field( 'sfc_pull_apply' ); ?>
					<?php submit_button( __( 'Pull & Apply Links', 'siloforge-connect' ), 'primary', 'submit', false ); ?>
				</form>
			</div>

			<h2><?php echo esc_html__( 'Last Sync', 'siloforge-connect' ); ?></h2>
			<?php if ( is_array( $last_sync ) && ! empty( $last_sync['totals'] ) ) : ?>
				<table class="widefat striped" style="max-width:600px;">
					<tbody>
						<tr>
							<td><?php echo esc_html__( 'Time (UTC)', 'siloforge-connect' ); ?></td>
							<td><?php echo esc_html( isset( $last_sync['time'] ) ? $last_sync['time'] : '' ); ?></td>
						</tr>
						<?php foreach ( array( 'posts', 'batches', 'received', 'upserted', 'skipped' ) as $stat_key ) : ?>
							<tr>
								<td><?php echo esc_html( ucfirst( $stat_key ) ); ?></td>
								<td><?php echo esc_html( isset( $last_sync['totals'][ $stat_key ] ) ? (string) (int) $last_sync['totals'][ $stat_key ] : '0' ); ?></td>
							</tr>
						<?php endforeach; ?>
					</tbody>
				</table>
			<?php else : ?>
				<p><?php echo esc_html__( 'No sync has run yet.', 'siloforge-connect' ); ?></p>
			<?php endif; ?>

			<h2><?php echo esc_html__( 'Reverting', 'siloforge-connect' ); ?></h2>
			<p>
				<?php echo esc_html__( 'Before the first change to a post, its original content is snapshotted to post meta (keys starting with _sfc_snapshot_, last 3 kept). To undo the most recent SiloForge change to a post, use SFC_Linker::revert_last( $post_id ) via WP-CLI (wp eval) or restore the snapshot meta value manually.', 'siloforge-connect' ); ?>
			</p>
		</div>
		<?php
	}

	/**
	 * Connection panel: backend reachability + version (GET /health) and auth status.
	 *
	 * @return void
	 */
	private function render_connection() {
		$health = $this->client->health();
		$status = $this->client->connection_status();
		$state  = $this->client->auth_state();

		if ( is_wp_error( $health ) ) {
			$backend = sprintf(
				/* translators: %s: error message. */
				__( 'Unreachable: %s', 'siloforge-connect' ),
				$health->get_error_message()
			);
			$backend_ok = false;
		} else {
			$version    = '' !== $health['version'] ? $health['version'] : __( 'unknown version', 'siloforge-connect' );
			$backend    = sprintf(
				/* translators: 1: version, 2: ok/degraded. */
				__( 'SiloForge %1$s (%2$s)', 'siloforge-connect' ),
				$version,
				$health['ok'] ? __( 'healthy', 'siloforge-connect' ) : __( 'degraded', 'siloforge-connect' )
			);
			$backend_ok = $health['ok'] && SFC_Client::backend_supported( $health['version'] );
		}

		$labels = array(
			'no_key'        => __( 'Not configured: enter the API URL and the site API key below.', 'siloforge-connect' ),
			'key_rejected'  => __( 'API key rejected. If the key was rotated in SiloForge (Sites -> Rotate key), paste the new key below and save.', 'siloforge-connect' ),
			'connected'     => __( 'Connected', 'siloforge-connect' ),
			'not_connected' => __( 'Not connected yet: click Test Connection.', 'siloforge-connect' ),
		);
		$auth_ok = ( 'connected' === $status );
		$since   = ( $auth_ok && isset( $state['time'] ) ) ? gmdate( 'Y-m-d H:i', (int) $state['time'] ) . ' UTC' : '';
		$pending = get_option( 'sfc_pending_acks', array() );
		$pending = is_array( $pending ) ? count( $pending ) : 0;
		?>
		<h2><?php echo esc_html__( 'Connection', 'siloforge-connect' ); ?></h2>
		<table class="widefat striped" style="max-width:760px;">
			<tbody>
				<tr>
					<td style="width:180px;"><?php echo esc_html__( 'Backend', 'siloforge-connect' ); ?></td>
					<td>
						<span class="dashicons <?php echo esc_attr( $backend_ok ? 'dashicons-yes-alt' : 'dashicons-warning' ); ?>"></span>
						<?php echo esc_html( $backend ); ?>
						<?php if ( ! is_wp_error( $health ) && '' !== $health['version'] && ! SFC_Client::backend_supported( $health['version'] ) ) : ?>
							<br /><strong><?php echo esc_html__( 'This plugin requires SiloForge 2.x. Upgrade the backend.', 'siloforge-connect' ); ?></strong>
						<?php endif; ?>
					</td>
				</tr>
				<tr>
					<td><?php echo esc_html__( 'Status', 'siloforge-connect' ); ?></td>
					<td>
						<span class="dashicons <?php echo esc_attr( $auth_ok ? 'dashicons-yes-alt' : 'dashicons-warning' ); ?>"></span>
						<?php echo esc_html( $labels[ $status ] ); ?>
						<?php if ( '' !== $since ) : ?>
							<?php
							/* translators: %s: time of the last successful handshake. */
							echo esc_html( sprintf( __( '(last handshake %s)', 'siloforge-connect' ), $since ) );
							?>
						<?php endif; ?>
					</td>
				</tr>
				<?php if ( $pending > 0 ) : ?>
					<tr>
						<td><?php echo esc_html__( 'Pending acks', 'siloforge-connect' ); ?></td>
						<td>
							<?php
							/* translators: %d: number of acknowledgements. */
							echo esc_html( sprintf( __( '%d applied link(s) not yet confirmed to SiloForge; they are re-sent on the next Pull & Apply.', 'siloforge-connect' ), $pending ) );
							?>
						</td>
					</tr>
				<?php endif; ?>
			</tbody>
		</table>
		<?php
	}

	/**
	 * Capability + nonce guard for admin-post handlers.
	 *
	 * @param string $action Nonce action.
	 * @return void
	 */
	private function guard( $action ) {
		if ( ! current_user_can( 'manage_options' ) ) {
			wp_die( esc_html__( 'You are not allowed to do that.', 'siloforge-connect' ) );
		}
		check_admin_referer( $action );
	}

	/**
	 * Queue an admin notice for the current user.
	 *
	 * @param string $type    'success' or 'error'.
	 * @param string $message Notice text.
	 * @return void
	 */
	private function set_notice( $type, $message ) {
		set_transient(
			'sfc_notice_' . get_current_user_id(),
			array(
				'type'    => $type,
				'message' => $message,
			),
			MINUTE_IN_SECONDS
		);
	}

	/**
	 * Redirect back to the settings page.
	 *
	 * @return void
	 */
	private function redirect_back() {
		wp_safe_redirect( admin_url( 'options-general.php?page=' . self::PAGE_SLUG ) );
		exit;
	}
}
