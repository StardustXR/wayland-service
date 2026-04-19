use crate::{client::Client, error::WaylandResult};
use waynest::ObjectId;
pub use waynest_protocols::server::core::wayland::wl_output::*;

#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct Output {
    pub id: ObjectId,
    pub version: u32,
}
impl Output {
    pub async fn advertise_outputs(&self, client: &mut Client) -> WaylandResult<()> {
        self.geometry(
            client,
            self.id,
            2048,
            2048,
            0,
            0,
            Subpixel::None,
            "Stardust Virtual Display".to_string(),
            "Stardust Virtual Display".to_string(),
            Transform::Normal,
        )
        .await?;

        if self.version >= 4 {
            self.name(client, self.id, "Stardust Virtual Display".to_string())
                .await?;
            self.description(
                client,
                self.id,
                "I needed this to account for dumb clients".to_string(),
            )
            .await?;
        }

        self.mode(
            client,
            self.id,
            Mode::Current | Mode::Preferred,
            2048,
            2048,
            // wayland reports this in millihertz apparently
            2048 * 1000,
        )
        .await?;

        if self.version >= 2 {
            self.done(client, self.id).await?;
        }
        Ok(())
    }
}
impl WlOutput for Output {
    type Connection = Client;

    /// https://wayland.app/protocols/wayland#wl_output:request:release
    async fn release(
        &self,
        _client: &mut Self::Connection,
        _sender_id: ObjectId,
    ) -> WaylandResult<()> {
        Ok(())
    }
}
