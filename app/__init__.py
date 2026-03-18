from flask import Flask
import os
from werkzeug.middleware.proxy_fix import ProxyFix


def create_app():
    app = Flask(__name__)    

    CURRENT_DIR = os.path.dirname(os.path.realpath(__file__))
    CONFIG_PATH = os.path.join(CURRENT_DIR, '..', 'config.py')

    app.wsgi_app = ProxyFix(app.wsgi_app, x_for=1, x_proto=1, x_host=1, x_port=1, x_prefix=1)
    
    # Use a fallback SECRET_KEY if config doesn't exist yet (setup wizard flow)
env_secret = os.environ.get('SECRET_KEY')

if env_secret:
    app.config['SECRET_KEY'] = env_secret
else:
    # fallback to config.py
    if os.path.exists(CONFIG_PATH):
        config_namespace = {}
        with open(CONFIG_PATH, 'r') as file:
            exec(file.read(), {}, config_namespace)
        app.config['SECRET_KEY'] = config_namespace.get('SECRET_KEY', 'setup-mode-temp-key')
    else:
        app.config['SECRET_KEY'] = 'setup-mode-temp-key'

    with app.app_context():
        from .routes import main_bp, init
        init()
        
        # Register blueprints
        app.register_blueprint(main_bp)

    return app
