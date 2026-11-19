# os.beecoders.com: the static site served by unprivileged nginx on port 8080.
# Built by scripts/deploy-k3s.sh (context: website/).
FROM docker.io/nginxinc/nginx-unprivileged:1.27-alpine

COPY deploy/nginx.conf /etc/nginx/conf.d/default.conf
COPY index.html commands.html style.css site.js /usr/share/nginx/html/
COPY img/ /usr/share/nginx/html/img/
COPY media/ /usr/share/nginx/html/media/

EXPOSE 8080
